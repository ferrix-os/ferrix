//! The display core's half of the conversation, as a state machine.
//!
//! A [`Session`] starts from an accepted HELLO. The core asks it for each
//! message it wants to send — [`Session::attach`], [`Session::scanout`],
//! [`Session::flush`], [`Session::cursor`], [`Session::move_cursor`],
//! [`Session::detach`], [`Session::stop`] — and the
//! session refuses a request that would put the conversation in a state the
//! protocol does not have, such as showing a buffer the device has not
//! attached, or detaching one a scanout still shows. Every message from the
//! driver goes through [`Session::receive`], which accepts only the reply the
//! session is waiting for: an ATTACHED for a buffer being attached, a FLIPPED
//! for the oldest flush in flight, a DETACHED for a buffer being detached, a
//! STOPPED after STOP -- and MODES, which the driver sends unasked when the
//! device's displays change, if it describes the card HELLO did. Anything
//! else is [`Refusal::Protocol`], after which
//! the session is broken and refuses everything, and the glue quiesces the
//! driver the way it quiesces a block driver that lies.
//!
//! Capacity is fixed, so the kernel side allocates nothing per message:
//! [`MAX_BUFFERS`] buffers and [`MAX_IN_FLIGHT`] flushes waiting for FLIPPED.

use crate::message::AttachObject;
use crate::message::{
    Attach, AttachError, CURSOR_SIZE, Hello, Listed, MAX_LISTED_MODES, MAX_REFRESH_MHZ,
    MAX_SCANOUTS, MODES_A_PAGE, Message, ModePage, Rect, Refusal, ScanoutMode, Status, Timing,
    validate_modes,
};
use ferrix_native_abi::rights::Rights;

/// The most buffers one session tracks, attached or on their way.
pub const MAX_BUFFERS: usize = 32;

/// The most flushes waiting for FLIPPED at once.
pub const MAX_IN_FLIGHT: usize = 8;

/// A request the core may not make now. The conversation is unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestError {
    /// The session is broken or stopping.
    Closed,
    /// The ATTACH is not one a driver can act on.
    Attach(AttachError),
    /// A buffer with that id is already tracked.
    InUse,
    /// [`MAX_BUFFERS`] are already tracked.
    Full,
    /// No attached buffer has that id.
    NotAttached,
    /// The buffer is on a scanout or has a flush in flight.
    Busy,
    /// [`MAX_IN_FLIGHT`] flushes are already waiting.
    TooManyFlushes,
    /// The scanout index is not below the HELLO's count.
    NoSuchScanout,
    /// The rectangle is empty or runs past the buffer.
    Rect,
    /// A cursor's buffer is not [`CURSOR_SIZE`] square, or its hotspot is
    /// outside it, or the card said it has no cursor plane.
    Cursor,
    /// `SET_MODE` for a scanout with no finished list, with a list that is
    /// not the one the driver sent, or with a timing that list does not
    /// hold.
    Mode,
}

/// What a driver's message meant, once accepted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// An ATTACH finished; on success the buffer may be shown and flushed.
    Attached {
        /// The buffer.
        buffer: u32,
        /// How it went.
        status: Status,
    },
    /// A flush finished: the page flip's completion event is due.
    Flipped {
        /// The flush.
        sequence: u64,
        /// The buffer it flushed.
        buffer: u32,
        /// How it went.
        status: Status,
    },
    /// A DETACH finished. On failure the range stays the device's, and the
    /// core must never hand it out again.
    Detached {
        /// The buffer.
        buffer: u32,
        /// How it went.
        status: Status,
    },
    /// The driver stopped.
    Stopped,
    /// The scanouts' preferred modes changed; [`Session::modes`] has the
    /// new ones.
    Modes,
    /// The driver sent `scanout`'s EDID; the message has the bytes.
    Edid {
        /// The scanout.
        scanout: u32,
    },
    /// A page of `scanout`'s mode list, which the message has: the first
    /// of a new list when `starts`, and the last when `finishes`, after
    /// which [`Session::set_mode`] takes the list the pages made.
    ModeList {
        /// The scanout.
        scanout: u32,
        /// Whether the page begins a list, discarding one half sent.
        starts: bool,
        /// Whether it finishes it.
        finishes: bool,
    },
}

/// A list being sent, page by page: where the next page starts, how long
/// the list says it is, how many preferred entries it has had, and the
/// hash of the entries so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Paging {
    hash: u64,
    next: u8,
    total: u8,
    preferred: u8,
}

/// The FNV-1a hash of a list's entries, so that the session can hold the
/// glue to the list the driver sent without keeping 64 timings a scanout
/// itself: [`Session`] lives on a kernel stack while it is made.
const HASH_START: u64 = 0xcbf2_9ce4_8422_2325;

fn hash_listed(hash: u64, listed: &Listed) -> u64 {
    let timing = &listed.timing;
    let words: [u32; 11] = [
        timing.clock_khz,
        u32::from(timing.hdisplay),
        u32::from(timing.hsync_start),
        u32::from(timing.hsync_end),
        u32::from(timing.htotal),
        u32::from(timing.vdisplay),
        u32::from(timing.vsync_start),
        u32::from(timing.vsync_end),
        u32::from(timing.vtotal),
        u32::from(timing.hsync_high) | u32::from(timing.vsync_high) << 1,
        u32::from(listed.preferred),
    ];
    words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .fold(hash, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// The hash [`Session`] keeps of a finished list: what the glue's copy of
/// it must hash to.
#[must_use]
pub fn list_hash(list: &[Listed]) -> u64 {
    list.iter().fold(HASH_START, hash_listed)
}

/// Whether a listed timing is one a display can run: a mode, at a refresh
/// of at most [`MAX_REFRESH_MHZ`].
fn listable(timing: &Timing) -> bool {
    timing.is_mode() && (1..=u64::from(MAX_REFRESH_MHZ)).contains(&timing.refresh_mhz())
}

/// What the session remembers of a buffer, whichever way it was made.
///
/// Its id and its shape, which is all the rules here are about: a rectangle
/// shown or flushed has to be inside it. Where its pixels *are* -- a VMO of
/// its own, or a resource the device already holds -- is the
/// driver's business once the buffer is made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Buffer {
    /// Its id.
    pub buffer: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Free,
    Attaching(Buffer),
    Attached(Buffer),
    Detaching(Buffer),
    /// A buffer whose DETACH the device refused: its pages may still be the
    /// device's, so the id and the slot are never used again.
    Lost(u32),
}

impl Slot {
    const fn buffer(&self) -> Option<u32> {
        match self {
            Self::Free => None,
            Self::Attaching(attach) | Self::Attached(attach) | Self::Detaching(attach) => {
                Some(attach.buffer)
            }
            Self::Lost(buffer) => Some(*buffer),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct InFlight {
    sequence: u64,
    buffer: u32,
}

/// The core's side of one driver's conversation.
#[derive(Clone, Debug)]
pub struct Session {
    modes: [ScanoutMode; MAX_SCANOUTS],
    scanouts: usize,
    card_bytes: u64,
    cursor: bool,
    slots: [Slot; MAX_BUFFERS],
    shown: [u32; MAX_SCANOUTS],
    flushes: [InFlight; MAX_IN_FLIGHT],
    flush_head: usize,
    flush_count: usize,
    next_sequence: u64,
    stopping: bool,
    broken: bool,
    /// Each scanout's list being sent, if one is.
    paging: [Option<Paging>; MAX_SCANOUTS],
    /// Each scanout's last finished list's hash, `None` before its first.
    lists: [Option<u64>; MAX_SCANOUTS],
    /// The hash of the mode `SET_MODE` last told each scanout, `None` when
    /// it is to be told again: before the first, after a new list, and
    /// after the driver refused the flush that followed one. A hash rather
    /// than the timing keeps the session small: it is made on a kernel
    /// stack.
    set: [Option<u64>; MAX_SCANOUTS],
}

impl Session {
    /// Accept a driver's HELLO, with the rights of the handles it came with,
    /// for a card whose buffers may each be up to `card_bytes` of their
    /// VMOs.
    pub fn accept(
        hello: &Hello,
        handle_rights: &[Rights],
        card_bytes: u64,
    ) -> Result<Self, Refusal> {
        hello.validate(handle_rights)?;
        Ok(Self {
            modes: hello.modes,
            scanouts: usize::from(hello.scanouts),
            card_bytes,
            cursor: hello.cursor,
            slots: [Slot::Free; MAX_BUFFERS],
            shown: [0; MAX_SCANOUTS],
            flushes: [InFlight {
                sequence: 0,
                buffer: 0,
            }; MAX_IN_FLIGHT],
            flush_head: 0,
            flush_count: 0,
            next_sequence: 1,
            stopping: false,
            broken: false,
            paging: [None; MAX_SCANOUTS],
            lists: [None; MAX_SCANOUTS],
            set: [None; MAX_SCANOUTS],
        })
    }

    /// Whether `scanout` has a finished mode list: a driver that sent one
    /// is the only kind told `SET_MODE`.
    #[must_use]
    pub fn has_list(&self, scanout: u32) -> bool {
        self.lists
            .get(scanout as usize)
            .is_some_and(Option::is_some)
    }

    /// `SET_MODE` `scanout` to `timing`, which must be in `list`, the glue's
    /// copy of the scanout's last finished list. `Ok(None)` when the
    /// scanout runs that mode already, as far as it was told.
    ///
    /// # Errors
    ///
    /// [`RequestError::Mode`] for a scanout without a finished list, a
    /// `list` that is not the one the driver sent, or a timing it does not
    /// hold; the conversation is unchanged.
    pub fn set_mode(
        &mut self,
        scanout: u32,
        timing: Timing,
        list: &[Listed],
    ) -> Result<Option<Message>, RequestError> {
        self.open()?;
        let index = scanout as usize;
        if index >= self.scanouts {
            return Err(RequestError::NoSuchScanout);
        }
        let held = self.lists.get(index).copied().flatten();
        if held != Some(list_hash(list)) || !list.iter().any(|listed| listed.timing == timing) {
            return Err(RequestError::Mode);
        }
        let told = hash_listed(
            HASH_START,
            &Listed {
                timing,
                preferred: false,
            },
        );
        let set = self.set.get_mut(index).ok_or(RequestError::NoSuchScanout)?;
        if *set == Some(told) {
            return Ok(None);
        }
        *set = Some(told);
        Ok(Some(Message::SetMode { scanout, timing }))
    }

    /// Forget the mode `scanout` was last told, so that the next
    /// [`Session::set_mode`] tells it again: the driver refused the flush
    /// that followed it, and runs whatever it ran before.
    pub fn mode_refused(&mut self, scanout: u32) {
        if let Some(set) = self.set.get_mut(scanout as usize) {
            *set = None;
        }
    }

    /// The scanouts' preferred modes, as the driver reported them.
    #[must_use]
    pub fn modes(&self) -> &[ScanoutMode] {
        self.modes.get(..self.scanouts).unwrap_or(&[])
    }

    /// Whether the card has a cursor plane, as its HELLO said.
    #[must_use]
    pub const fn has_cursor(&self) -> bool {
        self.cursor
    }

    /// Whether the driver broke the protocol.
    #[must_use]
    pub const fn is_broken(&self) -> bool {
        self.broken
    }

    fn open(&self) -> Result<(), RequestError> {
        if self.broken || self.stopping {
            return Err(RequestError::Closed);
        }
        Ok(())
    }

    fn find(&self, buffer: u32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.buffer() == Some(buffer))
    }

    fn attached(&self, buffer: u32) -> Result<Buffer, RequestError> {
        match self.find(buffer).and_then(|index| self.slots.get(index)) {
            Some(Slot::Attached(attach)) => Ok(*attach),
            _ => Err(RequestError::NotAttached),
        }
    }

    fn in_flight(&self) -> impl Iterator<Item = &InFlight> {
        (0..self.flush_count)
            .filter_map(|offset| self.flushes.get((self.flush_head + offset) % MAX_IN_FLIGHT))
    }

    /// Whether `buffer`'s ATTACH is waiting for ATTACHED.
    #[must_use]
    pub fn is_attaching(&self, buffer: u32) -> bool {
        self.find(buffer)
            .and_then(|index| self.slots.get(index))
            .is_some_and(|slot| matches!(slot, Slot::Attaching(_)))
    }

    /// ATTACH a buffer.
    pub fn attach(&mut self, attach: Attach) -> Result<Message, RequestError> {
        self.open()?;
        attach
            .validate(self.card_bytes)
            .map_err(RequestError::Attach)?;
        if self.find(attach.buffer).is_some() {
            return Err(RequestError::InUse);
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| **slot == Slot::Free)
            .ok_or(RequestError::Full)?;
        *slot = Slot::Attaching(Buffer {
            buffer: attach.buffer,
            width: attach.width,
            height: attach.height,
        });
        Ok(Message::Attach(attach))
    }

    /// `ATTACH_OBJ` a buffer whose pixels the device holds already.
    ///
    /// The same rules as [`Session::attach`] but for the range, which such a
    /// buffer has not got: an id no live buffer has, a shape the device can
    /// show, and room in the table.
    ///
    /// # Errors
    ///
    /// A request the conversation has no room or no state for.
    pub fn attach_object(&mut self, attach: AttachObject) -> Result<Message, RequestError> {
        self.open()?;
        attach.validate().map_err(RequestError::Attach)?;
        if self.find(attach.buffer).is_some() {
            return Err(RequestError::InUse);
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| **slot == Slot::Free)
            .ok_or(RequestError::Full)?;
        *slot = Slot::Attaching(Buffer {
            buffer: attach.buffer,
            width: attach.width,
            height: attach.height,
        });
        Ok(Message::AttachObject(attach))
    }

    /// Show `rect` of `buffer` on `scanout`, or with buffer 0, turn it off.
    pub fn scanout(
        &mut self,
        scanout: u32,
        buffer: u32,
        rect: Rect,
    ) -> Result<Message, RequestError> {
        self.open()?;
        let index = scanout as usize;
        if index >= self.scanouts {
            return Err(RequestError::NoSuchScanout);
        }
        if buffer != 0 {
            let attach = self.attached(buffer)?;
            if !rect.inside(attach.width, attach.height) {
                return Err(RequestError::Rect);
            }
        }
        let shown = self
            .shown
            .get_mut(index)
            .ok_or(RequestError::NoSuchScanout)?;
        *shown = buffer;
        Ok(Message::Scanout {
            scanout,
            buffer,
            rect,
        })
    }

    /// FLUSH `rect` of `buffer`.
    pub fn flush(&mut self, buffer: u32, rect: Rect) -> Result<Message, RequestError> {
        self.open()?;
        let attach = self.attached(buffer)?;
        if !rect.inside(attach.width, attach.height) {
            return Err(RequestError::Rect);
        }
        let sequence = self.queue_flush(buffer)?;
        Ok(Message::Flush {
            buffer,
            sequence,
            rect,
        })
    }

    /// Put a flush of `buffer` in line and answer its sequence number.
    fn queue_flush(&mut self, buffer: u32) -> Result<u64, RequestError> {
        if self.flush_count >= MAX_IN_FLIGHT {
            return Err(RequestError::TooManyFlushes);
        }
        let sequence = self.next_sequence;
        let at = (self.flush_head + self.flush_count) % MAX_IN_FLIGHT;
        let entry = self
            .flushes
            .get_mut(at)
            .ok_or(RequestError::TooManyFlushes)?;
        *entry = InFlight { sequence, buffer };
        self.flush_count += 1;
        self.next_sequence = sequence.wrapping_add(1).max(1);
        Ok(sequence)
    }

    /// CURSOR: show `buffer` -- [`CURSOR_SIZE`] square, or 0 for none -- as
    /// `scanout`'s cursor with its hotspot at `hot` and its top-left corner
    /// at `at`. It waits in the flushes' line, since its image is moved to
    /// the device as a flush's pixels are, and FLIPPED answers it.
    pub fn cursor(
        &mut self,
        scanout: u32,
        buffer: u32,
        hot: (u32, u32),
        at: (i32, i32),
    ) -> Result<Message, RequestError> {
        self.open()?;
        if !self.cursor {
            return Err(RequestError::Cursor);
        }
        if scanout as usize >= self.scanouts {
            return Err(RequestError::NoSuchScanout);
        }
        if buffer != 0 {
            let attach = self.attached(buffer)?;
            if (attach.width, attach.height) != (CURSOR_SIZE, CURSOR_SIZE) {
                return Err(RequestError::Cursor);
            }
        }
        if hot.0 >= CURSOR_SIZE || hot.1 >= CURSOR_SIZE {
            return Err(RequestError::Cursor);
        }
        let sequence = self.queue_flush(buffer)?;
        Ok(Message::Cursor {
            scanout,
            buffer,
            sequence,
            hot_x: hot.0,
            hot_y: hot.1,
            x: at.0,
            y: at.1,
        })
    }

    /// MOVE `scanout`'s cursor's top-left corner to `at`. Nothing answers
    /// it, and nothing about the session changes.
    pub fn move_cursor(&mut self, scanout: u32, at: (i32, i32)) -> Result<Message, RequestError> {
        self.open()?;
        if !self.cursor {
            return Err(RequestError::Cursor);
        }
        if scanout as usize >= self.scanouts {
            return Err(RequestError::NoSuchScanout);
        }
        Ok(Message::Move {
            scanout,
            x: at.0,
            y: at.1,
        })
    }

    /// DETACH a buffer no scanout shows and no flush is waiting on.
    pub fn detach(&mut self, buffer: u32) -> Result<Message, RequestError> {
        self.open()?;
        let attach = self.attached(buffer)?;
        if self.shown.contains(&buffer) || self.in_flight().any(|flush| flush.buffer == buffer) {
            return Err(RequestError::Busy);
        }
        let index = self.find(buffer).ok_or(RequestError::NotAttached)?;
        let slot = self.slots.get_mut(index).ok_or(RequestError::NotAttached)?;
        *slot = Slot::Detaching(attach);
        Ok(Message::Detach { buffer })
    }

    /// STOP the driver. Every request after this is refused.
    pub fn stop(&mut self) -> Result<Message, RequestError> {
        self.open()?;
        self.stopping = true;
        Ok(Message::Stop)
    }

    fn violation(&mut self) -> Refusal {
        self.broken = true;
        Refusal::Protocol
    }

    /// Accept a message from the driver, if it is one the session waits for.
    pub fn receive(&mut self, message: &Message) -> Result<Event, Refusal> {
        if self.broken {
            return Err(Refusal::Protocol);
        }
        match *message {
            Message::Attached { buffer, status } => self.on_attached(buffer, status),
            Message::Flipped { sequence, status } => self.on_flipped(sequence, status),
            Message::Detached { buffer, status } => self.on_detached(buffer, status),
            Message::Stopped if self.stopping => Ok(Event::Stopped),
            Message::Modes { modes } => self.on_modes(modes),
            // A scanout the driver has; the bytes are the core's to check
            // as it checks a file's, which the decode left as they came.
            Message::Edid(edid) if (edid.scanout as usize) < self.scanouts => Ok(Event::Edid {
                scanout: edid.scanout,
            }),
            Message::ModeList(page) => self.on_mode_list(&page),
            _ => Err(self.violation()),
        }
    }

    /// A page of a mode list, as `docs/DISPLAY.md` §9.2 has them: a scanout
    /// the card has; a total of 1 to [`MAX_LISTED_MODES`]; 1 to
    /// [`MODES_A_PAGE`] entries, each a mode at a refresh a display can
    /// have; a first page at 0 -- which discards a list half sent -- and
    /// every other where the last one ended, of the same total, and none
    /// past it; and exactly one preferred entry once the list is finished.
    fn on_mode_list(&mut self, page: &ModePage) -> Result<Event, Refusal> {
        let index = page.scanout as usize;
        let count = page.count as usize;
        let fits = index < self.scanouts
            && (1..=MAX_LISTED_MODES).contains(&(page.total as usize))
            && (1..=MODES_A_PAGE).contains(&count)
            && page
                .first
                .checked_add(page.count)
                .is_some_and(|end| end <= page.total)
            && page
                .as_slice()
                .iter()
                .all(|listed| listable(&listed.timing));
        if !fits {
            return Err(self.violation());
        }
        let starts = page.first == 0;
        // Both below 256: the checks above bound them by 64 and 24.
        let (Ok(first), Ok(total), Ok(added)) = (
            u8::try_from(page.first),
            u8::try_from(page.total),
            u8::try_from(page.count),
        ) else {
            return Err(self.violation());
        };
        let before = if starts {
            Paging {
                next: 0,
                total,
                preferred: 0,
                hash: HASH_START,
            }
        } else {
            match self.paging.get(index).copied().flatten() {
                Some(paging) if paging.next == first && paging.total == total => paging,
                _ => return Err(self.violation()),
            }
        };
        let preferred = page
            .as_slice()
            .iter()
            .filter(|listed| listed.preferred)
            .count();
        let paging = Paging {
            next: first.saturating_add(added),
            total,
            preferred: before
                .preferred
                .saturating_add(u8::try_from(preferred).unwrap_or(u8::MAX)),
            hash: page.as_slice().iter().fold(before.hash, hash_listed),
        };
        let finishes = page.finishes();
        if paging.preferred > 1 || (finishes && paging.preferred != 1) {
            return Err(self.violation());
        }
        if finishes {
            if let Some(slot) = self.paging.get_mut(index) {
                *slot = None;
            }
            if let Some(slot) = self.lists.get_mut(index) {
                *slot = Some(paging.hash);
            }
            // A new list may not hold the mode last set, and the driver
            // judges SET_MODE by its own list: tell it again.
            self.mode_refused(page.scanout);
        } else if let Some(slot) = self.paging.get_mut(index) {
            *slot = Some(paging);
        }
        Ok(Event::ModeList {
            scanout: page.scanout,
            starts,
            finishes,
        })
    }

    fn on_modes(&mut self, modes: [ScanoutMode; MAX_SCANOUTS]) -> Result<Event, Refusal> {
        let scanouts = u16::try_from(self.scanouts).unwrap_or(u16::MAX);
        if validate_modes(&modes, scanouts).is_err() {
            return Err(self.violation());
        }
        self.modes = modes;
        Ok(Event::Modes)
    }

    fn on_attached(&mut self, buffer: u32, status: Status) -> Result<Event, Refusal> {
        let Some(slot) = self
            .find(buffer)
            .and_then(|index| self.slots.get_mut(index))
        else {
            return Err(self.violation());
        };
        let Slot::Attaching(attach) = *slot else {
            return Err(self.violation());
        };
        *slot = if status == Status::Ok {
            Slot::Attached(attach)
        } else {
            Slot::Free
        };
        Ok(Event::Attached { buffer, status })
    }

    fn on_flipped(&mut self, sequence: u64, status: Status) -> Result<Event, Refusal> {
        let oldest = self.in_flight().next().copied();
        match oldest {
            Some(flush) if flush.sequence == sequence => {
                self.flush_head = (self.flush_head + 1) % MAX_IN_FLIGHT;
                self.flush_count -= 1;
                Ok(Event::Flipped {
                    sequence,
                    buffer: flush.buffer,
                    status,
                })
            }
            _ => Err(self.violation()),
        }
    }

    fn on_detached(&mut self, buffer: u32, status: Status) -> Result<Event, Refusal> {
        let Some(slot) = self
            .find(buffer)
            .and_then(|index| self.slots.get_mut(index))
        else {
            return Err(self.violation());
        };
        let Slot::Detaching(_) = *slot else {
            return Err(self.violation());
        };
        *slot = if status == Status::Ok {
            Slot::Free
        } else {
            Slot::Lost(buffer)
        };
        Ok(Event::Detached { buffer, status })
    }
}
