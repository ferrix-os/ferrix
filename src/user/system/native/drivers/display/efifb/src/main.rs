//! The firmware framebuffer's driver process: a ring-3 program that serves
//! the screen firmware left lit to the kernel's display core as one card
//! (`docs/DISPLAY.md` §8).
//!
//! A PC whose GPU has no driver still has the framebuffer its firmware's
//! graphics output drew the boot on, at the mode the firmware chose. That is
//! how every Linux live image starts before its GPU driver loads, and it is
//! all this driver has: no mode setting, no vertical blanking, no GPU.
//!
//! 1. START, as the HDMI driver's: the device -- the node the kernel
//!    publishes for the framebuffer under `ferrix.efifb` -- the driver's end
//!    of the display control channel, and the framebuffer's one window. The
//!    mode is in START's fields a tree node leaves zero (`TREE_FIRMWARE_FB`).
//! 2. The window is mapped write-combining, the one type the kernel lets a
//!    mapping of it have (its own, for a panic): the stores of a frame leave
//!    the processor in bursts, not one at a time. After READY, eight full
//!    frames are copied and timed, and the median said: the figure a real
//!    machine gives for a frame (`docs/DISPLAY.md` §8). HELLO offers one scanout
//!    at the firmware's size with that one timing listed, says it copies,
//!    and has no cursor plane. READY brings
//!    the card VMO back with `READ | MAP` and never `WRITE`, and it is mapped
//!    read-only, whole.
//! 3. ATTACH records a buffer's range: there is nothing to pin, and the
//!    kernel would refuse a pin of this node. SCANOUT shows a buffer, copying
//!    all of it that fits; FLUSH copies the rectangle that changed when it is
//!    of the buffer on screen, and FLIPPED answers it at once, since there is
//!    no blanking to wait for. Every copy ends with a store fence, after
//!    which, and not before, the frame counts as shown. DETACH forgets a buffer, taking it off the
//!    screen first if it is there.
//! 4. STOP, or the core closing its end, ends the run. What was copied last
//!    stays on the screen, and a panic is drawn over it by the kernel.
//!
//! The exit status names the step that failed ([`Step`]), 0 a clean STOP.

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::ptr;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES, Start};
use ferrix_displayctl::message::{
    Attach, Hello, MAX_BYTES, MAX_DIMENSION, MAX_SCANOUTS, Message, PORT_RIGHTS, Rect, ScanoutMode,
    Status, Timing, Timings, VERSION,
};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{
    FRAMEBUFFER_BGRX, FRAMEBUFFER_RGBX, IoMappingSpec, PACKET_SIGNAL, PACKET_USER, TREE_FIRMWARE_FB,
};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::Vmo;
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// A page.
const PAGE: usize = 4096;

/// Bytes a pixel, in the card's buffers and on the screen alike.
const PIXEL: usize = 4;

/// Buffers known at once: a compositor double- or triple-buffers.
const MAX_BUFFERS: usize = 8;

/// The port key the control channel's signals arrive under.
const KEY_CONTROL: u64 = 1;

/// Full frames copied and timed after READY, for the figure.
const TIMED_FRAMES: usize = 8;

/// The refresh the one timing is made for. Firmware says none; a timing
/// needs a clock, and 60 Hz is what every monitor runs.
const REFRESH_HZ: u64 = 60;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not the firmware's framebuffer, or a
    /// mode this driver cannot show.
    Identity = 2,
    /// The framebuffer's window could not be mapped.
    Window = 3,
    /// The card could not be mapped.
    Memory = 4,
    /// HELLO could not be sent, or READY did not come.
    Hello = 6,
    /// The port or the waits could not be arranged.
    Events = 7,
    /// The core sent something this driver cannot act on.
    Control = 8,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(()) => 0,
        Err(step) => step as i32,
    }
}

/// What START handed over.
struct Started {
    start: Start,
    device: Device<Kernel>,
    control: Channel<Kernel>,
}

fn started(boot: &Channel<Kernel>) -> Result<Started, Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 2];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 2 {
        return Err(Step::Start);
    }
    let [device, control] = handles;
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, device));
    let control = Channel::from_owned(OwnedHandle::from_raw(Kernel, control));
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(start)) => Ok(Started {
            start,
            device,
            control,
        }),
        _ => Err(Step::Start),
    }
}

/// The screen: the framebuffer's window, mapped, and its mode.
struct Screen {
    _mapping: IoMapping<Kernel>,
    /// Where its first pixel is mapped.
    base: usize,
    /// Bytes mapped from `base`.
    len: usize,
    width: usize,
    height: usize,
    /// Bytes from one line's start to the next.
    pitch: usize,
    /// Whether the pixels are red first, so each copied pixel is turned.
    rgbx: bool,
}

impl Screen {
    /// The screen START describes: its mode, checked against its window, and
    /// the window mapped write-combining.
    fn map(device: &Device<Kernel>, start: &Start) -> Result<Screen, Step> {
        if start.pci_device_id != TREE_FIRMWARE_FB {
            return Err(Step::Identity);
        }
        let width = usize::from(start.pci_subsystem_vendor_id);
        let height = usize::from(start.pci_subsystem_id);
        let stride = start.notify_off_multiplier as usize;
        let rgbx = match start.msix_table_size {
            FRAMEBUFFER_BGRX => false,
            FRAMEBUFFER_RGBX => true,
            _ => return Err(Step::Identity),
        };
        let largest = MAX_DIMENSION as usize;
        if width == 0 || height == 0 || width > largest || height > largest || stride < width {
            return Err(Step::Identity);
        }
        let block = &start.common;
        let len = block.length as usize;
        let pitch = stride * PIXEL;
        if block.offset != 0 || len == 0 || !len.is_multiple_of(PAGE) || pitch * height > len {
            return Err(Step::Identity);
        }
        let mapping = device
            .io_mapping(IoMappingSpec {
                phys: block.phys,
                len: len as u64,
            })
            .map_err(|_| Step::Window)?;
        // The kernel maps the lent window only write-combining
        // (L.device.39): a refusal here says so on the console, the line
        // wc-ctl-plain expects (docs/DISPLAY.md section 8).
        let base = mapping.map_combining(None).map_err(|_| {
            say(format_args!(
                "efifb: the kernel refused the window's mapping"
            ));
            Step::Window
        })?;
        Ok(Screen {
            _mapping: mapping,
            base,
            len,
            width,
            height,
            pitch,
            rgbx,
        })
    }

    /// The one mode, as HELLO lists it: the firmware's size at 60 Hz with
    /// reduced blanking, which is a timing the core can list and nothing
    /// this driver will ever set.
    fn timing(&self) -> Timing {
        let (width, height) = (self.width as u16, self.height as u16);
        let htotal = width + 160;
        let vtotal = height + 30;
        let clock_khz = u64::from(htotal) * u64::from(vtotal) * REFRESH_HZ / 1000;
        Timing {
            clock_khz: u32::try_from(clock_khz).unwrap_or(u32::MAX),
            hdisplay: width,
            hsync_start: width + 48,
            hsync_end: width + 80,
            htotal,
            vdisplay: height,
            vsync_start: height + 3,
            vsync_end: height + 9,
            vtotal,
            hsync_high: true,
            vsync_high: false,
        }
    }
}

/// Say HELLO for the one scanout at the firmware's mode and take READY's
/// card VMO.
fn introduce(
    control: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    screen: &Screen,
) -> Result<Vmo<Kernel>, Step> {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    if let Some(first) = modes.first_mut() {
        *first = ScanoutMode {
            width: screen.width as u32,
            height: screen.height as u32,
            enabled: true,
            // Firmware says no refresh, and this driver knows none.
            refresh_mhz: 0,
        };
    }
    let mut timings = Timings::NONE;
    if let Some(slot) = timings.list.first_mut() {
        *slot = screen.timing();
        timings.count = 1;
    }
    let hello = Hello {
        version: VERSION,
        scanouts: 1,
        location,
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        cursor: false,
        timings,
        // The framebuffer reads nothing on its own: every frame is copied.
        copies: true,
    };
    let share = port
        .as_owned()
        .duplicate(Requested::Exactly(PORT_RIGHTS))
        .map_err(|_| Step::Hello)?;
    control
        .write_with(Message::Hello(hello).encode().as_bytes(), [share])
        .map_err(|_| Step::Hello)?;
    let _ = control
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Hello)?;
    let mut bytes = [0_u8; MAX_BYTES];
    let mut handles = [Handle::INVALID; 2];
    let received = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Hello)?;
    let [card, core_port] = handles;
    match Message::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(Message::Ready(_)) if received.handles == 2 => {
            drop(OwnedHandle::from_raw(Kernel, core_port));
            Ok(Vmo::from_owned(OwnedHandle::from_raw(Kernel, card)))
        }
        _ => Err(Step::Control),
    }
}

fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let Started {
        start,
        device,
        control,
    } = started(boot)?;
    let screen = Screen::map(&device, &start)?;
    say(format_args!(
        "efifb: {}x{} firmware framebuffer, {} bytes a line, {}",
        screen.width,
        screen.height,
        screen.pitch,
        if screen.rgbx { "RGBX" } else { "BGRX" }
    ));
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    say(format_args!("efifb: window mapped write-combining"));
    let card = introduce(&control, &port, start.location, &screen)?;
    let card_bytes = card.size().map_err(|_| Step::Memory)?;
    let card_len = usize::try_from(card_bytes).map_err(|_| Step::Memory)?;
    let card_base = card
        .map(None, card_len, Protection::Read, 0)
        .map_err(|_| Step::Memory)?;
    time_frames(&screen, card_base, card_len);
    control
        .wait_async(&port, Signals::READABLE | Signals::PEER_CLOSED, KEY_CONTROL)
        .map_err(|_| Step::Events)?;

    let mut serving = Serving {
        screen,
        _card: card,
        card_base,
        card_bytes,
        control,
        port,
        _device: device,
        buffers: [None; MAX_BUFFERS],
        shown: None,
    };
    let ended = serving.serve();
    if matches!(ended, Ok(true)) {
        let _ = serving.control.write(Message::Stopped.encode().as_bytes());
    }
    ended.map(drop)
}

/// A buffer the screen may show: its range of the card and its shape.
#[derive(Clone, Copy)]
struct Buffer {
    id: u32,
    offset: usize,
    length: usize,
    /// Bytes from one row's start to the next.
    stride: usize,
    width: usize,
    height: usize,
}

/// The serve loop's state.
struct Serving {
    screen: Screen,
    /// The card VMO, held while its mapping is read.
    _card: Vmo<Kernel>,
    /// Where the card is mapped, read-only.
    card_base: usize,
    card_bytes: u64,
    control: Channel<Kernel>,
    port: Port<Kernel>,
    _device: Device<Kernel>,
    buffers: [Option<Buffer>; MAX_BUFFERS],
    /// The buffer on screen and the part of it shown, or `None`.
    shown: Option<(u32, Rect)>,
}

impl Serving {
    /// Serve until STOP (`true`) or the core closing its end (`false`).
    fn serve(&mut self) -> Result<bool, Step> {
        loop {
            let packet = self.port.wait(Deadline::Never).map_err(|_| Step::Events)?;
            if matches!(packet.kind, PACKET_SIGNAL | PACKET_USER) && packet.key == KEY_CONTROL {
                if let Some(stopped) = self.take_messages()? {
                    return Ok(stopped);
                }
                self.control
                    .wait_async(
                        &self.port,
                        Signals::READABLE | Signals::PEER_CLOSED,
                        KEY_CONTROL,
                    )
                    .map_err(|_| Step::Events)?;
            }
        }
    }

    fn reply(&self, message: Message) -> Result<(), Step> {
        self.control
            .write(message.encode().as_bytes())
            .map_err(|_| Step::Control)
    }

    /// Take the messages the core has sent until none is left. `Some` when
    /// the run is over.
    fn take_messages(&mut self) -> Result<Option<bool>, Step> {
        loop {
            let mut bytes = [0_u8; MAX_BYTES];
            match self.control.read(&mut bytes, &mut []) {
                Ok(received) => {
                    let message = Message::decode(bytes.get(..received.bytes).unwrap_or_default())
                        .map_err(|_| Step::Control)?;
                    match message {
                        Message::Stop => return Ok(Some(true)),
                        Message::Refused(_) => return Err(Step::Control),
                        other => self.act(other)?,
                    }
                }
                Err(ReadError::Failed(Error::PeerClosed)) => return Ok(Some(false)),
                Err(ReadError::Failed(Error::ShouldWait)) => return Ok(None),
                Err(_) => return Err(Step::Control),
            }
        }
    }

    /// Do what one message asks, and answer it.
    fn act(&mut self, message: Message) -> Result<(), Step> {
        match message {
            Message::Attach(attach) => {
                let status = self.attach(&attach);
                self.reply(Message::Attached {
                    buffer: attach.buffer,
                    status,
                })
            }
            // Pixels on a GPU this card does not have.
            Message::AttachObject(attach) => self.reply(Message::Attached {
                buffer: attach.buffer,
                status: Status::Invalid,
            }),
            Message::Scanout { buffer, rect, .. } => {
                self.scanout(buffer, rect);
                Ok(())
            }
            Message::Flush {
                buffer,
                sequence,
                rect,
            } => {
                if self.shown.is_some_and(|(id, _)| id == buffer) {
                    self.copy(buffer, rect);
                }
                // Nothing to wait for: the copy is on the screen.
                self.reply(Message::Flipped {
                    sequence,
                    status: Status::Ok,
                })
            }
            Message::Detach { buffer } => {
                let status = self.detach(buffer);
                self.reply(Message::Detached { buffer, status })
            }
            // No cursor plane was offered, so neither can come; and nothing
            // else is the core's to send now.
            _ => Err(Step::Control),
        }
    }

    /// Remember a buffer's range of the card; nothing is pinned.
    fn attach(&mut self, attach: &Attach) -> Status {
        if attach.validate(self.card_bytes).is_err() {
            return Status::Invalid;
        }
        if self.buffer(attach.buffer).is_some() {
            return Status::Invalid;
        }
        let Some(slot) = self.buffers.iter_mut().find(|slot| slot.is_none()) else {
            return Status::OutOfMemory;
        };
        let (Ok(offset), Ok(length)) = (
            usize::try_from(attach.offset),
            usize::try_from(attach.length),
        ) else {
            return Status::Invalid;
        };
        *slot = Some(Buffer {
            id: attach.buffer,
            offset,
            length,
            stride: attach.stride as usize,
            width: attach.width as usize,
            height: attach.height as usize,
        });
        Status::Ok
    }

    fn buffer(&self, id: u32) -> Option<Buffer> {
        self.buffers
            .iter()
            .flatten()
            .find(|buffer| buffer.id == id)
            .copied()
    }

    /// Show `rect` of a buffer from the screen's corner, copying all of it
    /// now; buffer 0, or one not attached, shows nothing new. The core does
    /// not wait for an answer to SCANOUT.
    fn scanout(&mut self, id: u32, rect: Rect) {
        if id == 0 || self.buffer(id).is_none() {
            self.shown = None;
            return;
        }
        self.shown = Some((id, rect));
        self.copy(id, rect);
    }

    /// Forget a buffer, taking it off the screen first if it is there.
    fn detach(&mut self, id: u32) -> Status {
        let Some(slot) = self
            .buffers
            .iter_mut()
            .find(|slot| slot.is_some_and(|buffer| buffer.id == id))
        else {
            return Status::Invalid;
        };
        *slot = None;
        if self.shown.is_some_and(|(shown, _)| shown == id) {
            self.shown = None;
        }
        Status::Ok
    }

    /// Copy `changed`, a rectangle of buffer `id` in the buffer's pixels, to
    /// the screen, where the scanout's rectangle puts it; whatever lies
    /// outside the buffer, the scanout's rectangle or the screen is left.
    /// The fence after it makes the copy shown.
    fn copy(&self, id: u32, changed: Rect) {
        self.copy_rows(id, changed);
        shown();
    }

    /// [`Serving::copy`]'s stores, without the fence.
    fn copy_rows(&self, id: u32, changed: Rect) {
        let (Some(buffer), Some((_, shown))) = (self.buffer(id), self.shown) else {
            return;
        };
        let Some(area) = Area::of(&buffer, shown, changed, &self.screen) else {
            return;
        };
        let card_end = self.card_bytes as usize;
        let screen = &self.screen;
        for row in 0..area.rows {
            let from =
                buffer.offset + (area.source_y + row) * buffer.stride + area.source_x * PIXEL;
            let to = (area.screen_y + row) * screen.pitch + area.screen_x * PIXEL;
            let bytes = area.columns * PIXEL;
            if from + bytes > buffer.offset + buffer.length
                || from + bytes > card_end
                || to + bytes > screen.len
            {
                return;
            }
            let source = (self.card_base + from) as *const u32;
            let target = (screen.base + to) as *mut u32;
            for column in 0..area.columns {
                // SAFETY: `source` is inside the buffer's range of the card
                // mapping, which is read-only and lives as long as `self`;
                // `target` inside the framebuffer window mapped for this
                // process. Both were bounds-checked for the whole row above,
                // and both are four-byte aligned: the card's ranges are whole
                // pages, a buffer's stride is a multiple of four (ATTACH's
                // validation), and so is the screen's pitch.
                let (from, to) = (source.wrapping_add(column), target.wrapping_add(column));
                // SAFETY: as above, `from` is a readable pixel of the card.
                let pixel = unsafe { ptr::read_volatile(from) };
                // SAFETY: as above, `to` is a pixel of the mapped window.
                unsafe { ptr::write_volatile(to, turned(pixel, screen.rgbx)) };
            }
        }
    }
}

/// Make every store to the write-combining window visible before what
/// follows -- FLIPPED, above all: the runtime's device barrier, `mfence` on
/// x86-64 (a full fence, which drains the write-combining buffers as
/// `sfence` does, with no assembly of this program's own).
fn shown() {
    ferrix_rt::device_barrier();
}

/// Copy the first full frame of the card to the screen [`TIMED_FRAMES`]
/// times, each ended by the fence, and say the median: the frame-copy
/// figure (`docs/DISPLAY.md` §8). What the card holds before any buffer is
/// attached is shown for those few frames and overwritten by the first.
/// In QEMU the number says nothing about write-combining; on a PC it is
/// what a frame of the software renderer costs to show.
fn time_frames(screen: &Screen, card_base: usize, card_len: usize) {
    let row = screen.width * PIXEL;
    if card_len < row * screen.height {
        say(format_args!(
            "efifb: full-frame copy not timed: the card is smaller than a frame"
        ));
        return;
    }
    let mut times = [0_u64; TIMED_FRAMES];
    for time in &mut times {
        let Ok(before) = ferrix_rt::linux::monotonic_nanos() else {
            return;
        };
        for line in 0..screen.height {
            let source = (card_base + line * row) as *const u32;
            let target = (screen.base + line * screen.pitch) as *mut u32;
            for column in 0..screen.width {
                // SAFETY: the card is mapped readable for `card_len` bytes,
                // checked above to hold a frame of `width` pixels a row; the
                // window holds `height` lines of `pitch` bytes (START's
                // check in `Screen::map`); both are four-byte aligned.
                let pixel = unsafe { ptr::read_volatile(source.wrapping_add(column)) };
                // SAFETY: as above, a pixel of the mapped window.
                unsafe {
                    ptr::write_volatile(target.wrapping_add(column), turned(pixel, screen.rgbx));
                }
            }
        }
        shown();
        let Ok(after) = ferrix_rt::linux::monotonic_nanos() else {
            return;
        };
        *time = after.saturating_sub(before);
    }
    times.sort_unstable();
    let median = times.get(TIMED_FRAMES / 2).copied().unwrap_or(0);
    say(format_args!(
        "efifb: full-frame copy {}x{}: median {} us over {} (write-combining)",
        screen.width,
        screen.height,
        median / 1000,
        TIMED_FRAMES
    ));
}

/// A pixel of the card's `XRGB8888` as the screen holds it: unchanged on a
/// BGRX screen, with red and blue exchanged on an RGBX one.
const fn turned(pixel: u32, rgbx: bool) -> u32 {
    if rgbx {
        (pixel & 0xFF00_FF00) | ((pixel & 0x00FF_0000) >> 16) | ((pixel & 0x0000_00FF) << 16)
    } else {
        pixel
    }
}

/// The part of a change that reaches the screen.
struct Area {
    /// Its top-left corner in the buffer.
    source_x: usize,
    source_y: usize,
    /// Its top-left corner on the screen.
    screen_x: usize,
    screen_y: usize,
    columns: usize,
    rows: usize,
}

impl Area {
    /// `changed`, clipped to the buffer, to `shown` (the part of the buffer
    /// on screen, with its corner at the screen's), and to the screen.
    fn of(buffer: &Buffer, shown: Rect, changed: Rect, screen: &Screen) -> Option<Area> {
        let clip = |start: u32, length: u32, low: usize, high: usize| {
            let end = (start as usize).saturating_add(length as usize).min(high);
            let start = (start as usize).max(low);
            (start < end).then_some((start, end))
        };
        let shown_x = shown.x as usize;
        let shown_y = shown.y as usize;
        let right = buffer
            .width
            .min(shown_x.saturating_add(shown.width as usize))
            .min(shown_x.saturating_add(screen.width));
        let bottom = buffer
            .height
            .min(shown_y.saturating_add(shown.height as usize))
            .min(shown_y.saturating_add(screen.height));
        let (left, right) = clip(changed.x, changed.width, shown_x, right)?;
        let (top, bottom) = clip(changed.y, changed.height, shown_y, bottom)?;
        Some(Area {
            source_x: left,
            source_y: top,
            screen_x: left - shown_x,
            screen_y: top - shown_y,
            columns: right - left,
            rows: bottom - top,
        })
    }
}

/// A line on standard error, which is the console.
fn say(arguments: fmt::Arguments<'_>) {
    let mut line = Line::default();
    let _ = line.write_fmt(arguments);
    let _ = line.write_str("\n");
    let _ = ferrix_rt::linux::write(2, line.as_bytes());
}

struct Line {
    bytes: [u8; 160],
    len: usize,
}

impl Default for Line {
    fn default() -> Self {
        Line {
            bytes: [0; 160],
            len: 0,
        }
    }
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}
