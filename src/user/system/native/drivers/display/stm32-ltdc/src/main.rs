//! The STM32MP15 DK board's HDMI driver process: a ring-3 program that
//! serves one card to the kernel's display core (`docs/DISPLAY.md` §6).
//!
//! Everything that decides a register value is `ferrix-stm32-display`,
//! tested on the host. This program is the handles it was written to be
//! wrapped in:
//!
//! 1. START, as the virtio-gpu driver's: the device, the driver's end of the
//!    display control channel, and the device's two register windows -- the
//!    LTDC's where virtio's common block would be, the HDMI bridge's I2C
//!    controller's where its device block would be.
//! 2. The bridge is found on the bus and its monitor's EDID read: whether to
//!    speak HDMI or DVI, and every mode the monitor offers. The driver says
//!    on standard error, which is the console, what the monitor is, its EDID
//!    in hex, and each mode with its pixel clock and whether the board can
//!    make it; the largest the board can run is the one it runs
//!    (`ferrix_stm32_display::choice`), 720p60 when nothing larger is. The
//!    kernel sets the pixel clock for it (`device_clock`), the LTDC starts
//!    it with its layer off, the bridge is told the mode and its TMDS output
//!    turned on. HELLO then offers the one scanout at that size with every
//!    mode the board can run on the monitor listed, and READY brings the
//!    card VMO back.
//! 3. ATTACH pins a buffer's range read-only and requires it to be one run
//!    of addresses, which the kernel makes it for this device. SCANOUT points
//!    the layer at it -- and when its rectangle is the size of another mode
//!    listed, switches to that mode first: TMDS off, the LTDC stopped, the
//!    clock set, both started again, which is how a `monitor =` line asking
//!    for 1280x720 on a monitor started at 1920x1080 gets it. FLUSH asks for
//!    a reload at the next vertical blanking
//!    and FLIPPED goes out when the reload's interrupt comes, so a
//!    compositor's page flips are paced by the screen. DETACH waits for a
//!    buffer on screen to leave it before it unpins.
//! 4. HELLO says the card has a cursor plane: the LTDC's second layer. A
//!    CURSOR points the layer at an attached 64x64 buffer, premultiplied
//!    ARGB8888 blended over the frame, and is answered at the reload like a
//!    flush; a MOVE rewrites the layer's window, clipped at the screen's
//!    edges, and is answered by nobody. Either lands at the next vertical
//!    blanking together with whatever flip waits for it, so the pointer
//!    moves at the screen's rate however long the compositor's frames take.
//!    A mode switch puts the pointer back where it was.
//! 5. STOP, or the core closing its end, turns the controller off -- nothing
//!    is read from memory after -- and TMDS with it.
//!
//! The exit status names the step that failed ([`Step`]), 0 a clean STOP.

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::ptr;

use ferrix_blkring::control::{Block as StartBlock, Message as StartMessage, START_BYTES, Start};
use ferrix_displayctl::message::{
    Attach, CURSOR_SIZE, Hello, MAX_BUFFER_PAGES as MAX_PAGES, MAX_BYTES, MAX_REFRESH_MHZ,
    MAX_SCANOUTS, MAX_TIMINGS, Message, PORT_RIGHTS, Rect, ScanoutMode, Status, Timing, Timings,
    VERSION,
};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{
    IoMappingSpec, PACKET_INTERRUPT, PACKET_SIGNAL, PACKET_USER, TREE_STM32_HDMI,
};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_stm32_display::choice::{self, Candidate, How, Runnable, Verdict};
use ferrix_stm32_display::edid::{Offer, Source, Unusable};
use ferrix_stm32_display::i2c::{I2c, TIMING_100KHZ_AT_64MHZ};
use ferrix_stm32_display::ltdc::{CursorImage, Frame, Ltdc};
use ferrix_stm32_display::mode::Mode;
use ferrix_stm32_display::sii9022::{self, Bridge};
use ferrix_stm32_display::{Budget, Registers, edid};

ferrix_rt::entry!(main);

/// A page.
const PAGE: usize = 4096;

/// The most pages one buffer may have, as the protocol says.
const MAX_BUFFER_PAGES: usize = MAX_PAGES as usize;

/// Buffers pinned at once: a compositor double- or triple-buffers.
const MAX_BUFFERS: usize = 8;

/// Flushes and CURSORs waiting for their reload at once. The core waits for
/// each FLIPPED before its caller goes on, so a compositor's frame and its
/// pointer's image are two at most.
const MAX_PENDING: usize = 8;

/// Port keys.
const KEY_INTERRUPT: u64 = 1;
const KEY_CONTROL: u64 = 2;

/// How long a buffer leaving the screen may take to go: one frame is
/// 16.7 ms at 60 Hz and 41.7 at 1080p's slowest, 24 Hz, and a register
/// read is well under a microsecond.
const LEAVE_BUDGET: Budget = Budget(5_000_000);

/// The mode the board runs when it can run nothing it chose: the one the
/// firmware's pixel clock is set for.
const FALLBACK: Mode = Mode::CEA_720P60;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not a DK board's HDMI output.
    Identity = 2,
    /// A register window could not be mapped.
    Registers = 3,
    /// Memory could not be made or mapped.
    Memory = 4,
    /// The LTDC is not a version this driver knows.
    Controller = 5,
    /// HELLO could not be sent, or READY did not come.
    Hello = 6,
    /// The port, the interrupt or the waits could not be arranged.
    Events = 7,
    /// The core sent something this driver cannot act on.
    Control = 8,
    /// Nothing answered at the bridge's address: unpowered, in reset, or
    /// the bus's pins are not the controller's.
    BridgeSilent = 20,
    /// Something answered, but not a `SiI902x`.
    BridgeChip = 21,
    /// The bridge would not take the mode.
    BridgeMode = 22,
    /// Another bus failure talking to the bridge.
    BridgeBus = 23,
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

// ---------------------------------------------------------------------------
// Registers
// ---------------------------------------------------------------------------

/// One register window, mapped.
struct Window {
    _mapping: IoMapping<Kernel>,
    base: usize,
    len: usize,
}

impl Window {
    fn map(device: &Device<Kernel>, block: &StartBlock) -> Result<Window, Step> {
        let len = block.length as usize;
        if block.offset != 0 || len == 0 || !len.is_multiple_of(PAGE) {
            return Err(Step::Registers);
        }
        let mapping = device
            .io_mapping(IoMappingSpec {
                phys: block.phys,
                len: len as u64,
            })
            .map_err(|_| Step::Registers)?;
        let base = mapping.map(None).map_err(|_| Step::Registers)?;
        Ok(Window {
            _mapping: mapping,
            base,
            len,
        })
    }

    fn address(&self, offset: u32) -> usize {
        let offset = offset as usize;
        assert!(
            offset + 4 <= self.len && offset.is_multiple_of(4),
            "a register inside the window, aligned"
        );
        self.base + offset
    }
}

impl Registers for Window {
    fn read32(&self, offset: u32) -> u32 {
        // SAFETY: mapped device memory the kernel gave this process, the
        // offset inside it and aligned, read volatile.
        unsafe { ptr::read_volatile(self.address(offset) as *const u32) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { ptr::write_volatile(self.address(offset) as *mut u32, value) }
    }
}

/// Room for a buffer's page addresses as the pin query writes them: 64 KiB,
/// too much for a stack.
struct Scratch {
    _vmo: Vmo<Kernel>,
    base: usize,
}

impl Scratch {
    fn new() -> Result<Scratch, Step> {
        let bytes = (MAX_BUFFER_PAGES * 8).div_ceil(PAGE) * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Scratch { _vmo: vmo, base })
    }

    fn raw(&mut self, pages: usize) -> &mut [[u8; 8]] {
        // SAFETY: `base` starts a zero-filled private mapping of
        // `MAX_BUFFER_PAGES` eight-byte arrays, which need no alignment;
        // `pages` is capped to that; `&mut self` keeps the slice unique.
        unsafe {
            core::slice::from_raw_parts_mut(self.base as *mut [u8; 8], pages.min(MAX_BUFFER_PAGES))
        }
    }
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

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

/// Find the bridge on the bus.
fn bridge(i2c: I2c<Window>) -> Result<Bridge<I2c<Window>>, Step> {
    use sii9022::{BridgeError, BusError};
    Bridge::probe(i2c, sii9022::ADDRESS).map_err(|error| match error {
        BridgeError::Bus(BusError::Nack) => Step::BridgeSilent,
        BridgeError::Chip(_) => Step::BridgeChip,
        _ => Step::BridgeBus,
    })
}

/// The rate in kHz the kernel would run the pixel clock at for `khz`.
fn rounded(device: &Device<Kernel>, khz: u32) -> Option<u32> {
    device
        .clock(khz.saturating_mul(1000), false)
        .ok()
        .map(|hz| hz / 1000)
}

/// Have the kernel run the pixel clock for `mode`: whether it now does,
/// within the half a percent a sink takes.
fn set_clock(device: &Device<Kernel>, mode: &Mode) -> bool {
    device
        .clock(mode.clock_khz.saturating_mul(1000), true)
        .is_ok_and(|hz| choice::close_enough(mode.clock_khz, hz / 1000))
}

/// Ask the monitor what it is and what it takes: whether it speaks HDMI (an
/// HDMI vendor block in its CTA-861 extension) or only DVI, and the modes
/// the board can run on it, all said on the console as they are found.
/// `None` for the modes when there is no EDID to read, which leaves the
/// board at 720p60 as before it read any.
fn monitor(bridge: &mut Bridge<I2c<Window>>, device: &Device<Kernel>) -> (bool, Option<Runnable>) {
    let mut base = [0_u8; edid::BLOCK_BYTES];
    let mut extension = [0_u8; edid::BLOCK_BYTES];
    if bridge.read_edid(0, &mut base).is_err() || !edid::is_base(&base) {
        say(format_args!(
            "ltdc: the monitor gave no EDID; DVI at 1280x720, 60 Hz"
        ));
        return (false, None);
    }
    let extensions = edid::extensions(&base);
    // Blocks past the second need the E-DDC segment pointer, which the
    // bridge's pass-through is not told; nearly every monitor has at most
    // one extension.
    let read = extensions > 0 && bridge.read_edid(1, &mut extension).is_ok();
    let following: &[[u8; edid::BLOCK_BYTES]] = if read {
        core::slice::from_ref(&extension)
    } else {
        &[]
    };
    let hdmi = following.first().is_some_and(edid::is_hdmi);
    tell_edid(&base, following, extensions, hdmi);
    let offers = edid::offers(&base, following);
    let mut round = |khz: u32| rounded(device, khz);
    for offer in offers.as_slice() {
        tell_offer(offer, &mut round);
    }
    if offers.dropped() > 0 {
        say(format_args!(
            "ltdc: {} more offered modes not read",
            offers.dropped()
        ));
    }
    let runnable = choice::runnable(
        &offers,
        edid::range_limits(&base),
        edid::continuous(&base),
        round,
    );
    for candidate in runnable.as_slice() {
        say(format_args!(
            "ltdc: can run {}{}",
            Described(&candidate.mode),
            match candidate.how {
                How::Listed => "",
                How::Retimed => ", retimed inside the monitor's range limits",
                How::Assumed => ", as every HDMI sink does though this one does not list it",
            }
        ));
    }
    (hdmi, Some(runnable))
}

/// The mode to start with: the largest the board can run, with the clock
/// set for it, or 720p60 with the clock set back for that.
fn first_mode(device: &Device<Kernel>, runnable: &mut Option<Runnable>) -> Mode {
    let Some(chosen) = runnable.as_ref().and_then(Runnable::chosen) else {
        *runnable = None;
        // A driver started again after one that set another rate finds that
        // rate still there: 720p60's is asked for, not assumed.
        let _ = set_clock(device, &FALLBACK);
        return FALLBACK;
    };
    if set_clock(device, &chosen.mode) {
        return chosen.mode;
    }
    say(format_args!(
        "ltdc: the kernel would not set the pixel clock for {}; 720p60 instead",
        Described(&chosen.mode)
    ));
    // The mode list goes with it: a card that cannot set its clock runs
    // the one mode it has the clock for.
    *runnable = None;
    let _ = set_clock(device, &FALLBACK);
    FALLBACK
}

/// A mode as DRM's list gives it to the core.
fn timing(mode: &Mode) -> Timing {
    Timing {
        clock_khz: mode.clock_khz,
        hdisplay: mode.hdisplay,
        hsync_start: mode.hsync_start,
        hsync_end: mode.hsync_end,
        htotal: mode.htotal,
        vdisplay: mode.vdisplay,
        vsync_start: mode.vsync_start,
        vsync_end: mode.vsync_end,
        vtotal: mode.vtotal,
        hsync_high: mode.hsync_high,
        vsync_high: mode.vsync_high,
    }
}

/// How many frames a second `mode` shows, in millihertz, rounded: 0 for a
/// mode with no frame, or one faster than a scanout may say.
fn refresh_mhz(mode: &Mode) -> u32 {
    let frame = u64::from(mode.htotal) * u64::from(mode.vtotal);
    if frame == 0 {
        return 0;
    }
    u32::try_from((u64::from(mode.clock_khz) * 1_000_000 + frame / 2) / frame)
        .ok()
        .filter(|&rate| rate <= MAX_REFRESH_MHZ)
        .unwrap_or(0)
}

/// Say HELLO for the one scanout, running `mode`, with the modes the board
/// can run listed (`mode` first), and take READY's card VMO.
fn introduce(
    control: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    mode: &Mode,
    runnable: Option<&Runnable>,
    cursor: bool,
) -> Result<Vmo<Kernel>, Step> {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    if let Some(first) = modes.first_mut() {
        *first = ScanoutMode {
            width: u32::from(mode.hdisplay),
            height: u32::from(mode.vdisplay),
            enabled: true,
            // What the running timing's clock makes of its frame, so that
            // the scanout says its refresh as a virtio-gpu's does.
            refresh_mhz: refresh_mhz(mode),
        };
    }
    let mut timings = Timings::NONE;
    let listed = runnable.map_or(&[][..], Runnable::as_slice);
    let others = listed
        .iter()
        .map(|c| c.mode)
        .filter(|m| !m.same_timing(mode));
    for (slot, each) in timings
        .list
        .iter_mut()
        .zip(core::iter::once(*mode).chain(others))
        .take(MAX_TIMINGS)
    {
        *slot = timing(&each);
        timings.count += 1;
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
        // The LTDC's second layer is one: the pointer moves without a
        // frame (`Serving::cursor`).
        cursor,
        timings,
        // The LTDC reads the pinned pages itself.
        copies: false,
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
    if start.pci_device_id != TREE_STM32_HDMI {
        return Err(Step::Identity);
    }
    let ltdc_window = Window::map(&device, &start.common)?;
    let i2c_window = Window::map(&device, &start.device)?;
    let interrupt = device.interrupt(0).map_err(|_| Step::Events)?;
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    interrupt
        .bind(&port, KEY_INTERRUPT)
        .map_err(|_| Step::Events)?;

    let mut ltdc = Ltdc::new(ltdc_window).map_err(|_| Step::Controller)?;
    let mut bridge = bridge(I2c::new(i2c_window, TIMING_100KHZ_AT_64MHZ))?;
    let (hdmi, mut runnable) = monitor(&mut bridge, &device);
    let mode = first_mode(&device, &mut runnable);
    say(format_args!(
        "ltdc: running {}, {}",
        Described(&mode),
        if hdmi { "HDMI" } else { "DVI" }
    ));
    // The bridge learns the mode with its output off, the controller starts
    // sending it, and only then does the monitor see a signal.
    bridge.set_mode(&mode, hdmi).map_err(|_| Step::BridgeMode)?;
    ltdc.start(&mode).map_err(|_| Step::Controller)?;
    bridge.enable().map_err(|_| Step::BridgeBus)?;

    let cursor = ltdc.has_cursor_layer();
    if cursor {
        say(format_args!(
            "ltdc: a {CURSOR_SIZE}x{CURSOR_SIZE} cursor plane on the second layer"
        ));
    }
    let card = match introduce(
        &control,
        &port,
        start.location,
        &mode,
        runnable.as_ref(),
        cursor,
    ) {
        Ok(card) => card,
        Err(step) => {
            ltdc.stop();
            let _ = bridge.disable();
            return Err(step);
        }
    };
    control
        .wait_async(&port, Signals::READABLE | Signals::PEER_CLOSED, KEY_CONTROL)
        .map_err(|_| Step::Events)?;

    let mut serving = Serving {
        ltdc,
        bridge,
        hdmi,
        runnable,
        interrupt,
        card,
        device,
        control,
        port,
        scratch: Scratch::new()?,
        buffers: [const { None }; MAX_BUFFERS],
        shown: None,
        pointer: Pointer::default(),
        pending: [0; MAX_PENDING],
        waiting: 0,
    };
    let ended = serving.serve();
    serving.ltdc.stop();
    let _ = serving.bridge.disable();
    if matches!(ended, Ok(true)) {
        let _ = serving.control.write(Message::Stopped.encode().as_bytes());
    }
    ended.map(drop)
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// A buffer the device may be shown.
struct Buffer {
    id: u32,
    _pin: Pin<Kernel>,
    /// Bus address of its first byte.
    address: u32,
    stride: u32,
    width: u32,
    height: u32,
}

/// The pointer on the LTDC's second layer, as CURSOR and MOVE last left
/// it: kept here, since a mode switch turns the layer off and the pointer
/// has to come back on the new mode's screen where it was.
#[derive(Clone, Copy, Default)]
struct Pointer {
    /// The buffer holding its image, or `None` for no pointer.
    buffer: Option<u32>,
    /// Where the image's top-left corner is, in the screen's pixels.
    at: (i32, i32),
}

/// The serve loop's state.
struct Serving {
    ltdc: Ltdc<Window>,
    bridge: Bridge<I2c<Window>>,
    /// Whether the sink speaks HDMI, which the bridge is told at each mode.
    hdmi: bool,
    /// The modes the board can run on this monitor, for a SCANOUT of
    /// another size; `None` runs only the mode it started with.
    runnable: Option<Runnable>,
    interrupt: Interrupt<Kernel>,
    card: Vmo<Kernel>,
    device: Device<Kernel>,
    control: Channel<Kernel>,
    port: Port<Kernel>,
    scratch: Scratch,
    buffers: [Option<Buffer>; MAX_BUFFERS],
    /// The buffer on screen, or `None` with the layer off.
    shown: Option<u32>,
    /// The pointer on the second layer.
    pointer: Pointer,
    /// Flush and CURSOR sequences waiting for the next reload, oldest
    /// first.
    pending: [u64; MAX_PENDING],
    waiting: usize,
}

impl Serving {
    /// Serve until STOP (`true`) or the core closing its end (`false`).
    fn serve(&mut self) -> Result<bool, Step> {
        loop {
            let packet = self.port.wait(Deadline::Never).map_err(|_| Step::Events)?;
            match (packet.kind, packet.key) {
                (PACKET_INTERRUPT, KEY_INTERRUPT) => {
                    let events = self.ltdc.take_events();
                    let _ = self.interrupt.ack();
                    if events.reloaded {
                        self.flipped()?;
                    }
                }
                (PACKET_SIGNAL | PACKET_USER, KEY_CONTROL) => {
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
                _ => {}
            }
        }
    }

    /// Answer every flush waiting for a reload: one has happened.
    fn flipped(&mut self) -> Result<(), Step> {
        let waiting = self.waiting;
        self.waiting = 0;
        for &sequence in self.pending.iter().take(waiting) {
            self.reply(Message::Flipped {
                sequence,
                status: Status::Ok,
            })?;
        }
        Ok(())
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
            Message::Scanout { buffer, rect, .. } => self.scanout(buffer, rect),
            Message::Flush {
                buffer, sequence, ..
            } => self.flush(buffer, sequence),
            // The one scanout is the only one the core lets through; the
            // hotspot is the core's business, the place already the image's
            // corner.
            Message::Cursor {
                buffer,
                sequence,
                x,
                y,
                ..
            } => self.cursor(buffer, sequence, (x, y)),
            Message::Move { x, y, .. } => {
                self.move_pointer((x, y));
                Ok(())
            }
            Message::Detach { buffer } => {
                let status = self.detach(buffer);
                self.reply(Message::Detached { buffer, status })
            }
            _ => Err(Step::Control),
        }
    }

    /// Pin a buffer's range and require it to be one run of addresses below
    /// 4 GiB, which is all the LTDC reads.
    fn attach(&mut self, attach: &Attach) -> Status {
        if attach.validate(self.card_bytes()).is_err() {
            return Status::Invalid;
        }
        let Some(slot) = self.buffers.iter().position(Option::is_none) else {
            return Status::OutOfMemory;
        };
        let (Ok(offset), Ok(length)) = (
            usize::try_from(attach.offset),
            usize::try_from(attach.length),
        ) else {
            return Status::Invalid;
        };
        let pages = length / PAGE;
        let Ok(pin) = self
            .device
            .pin(&self.card, offset, length, PinAccess::ReadOnly)
        else {
            return Status::PinFailed;
        };
        let raw = self.scratch.raw(pages);
        let complete = pin
            .addresses(raw)
            .is_ok_and(|got| got.pages == pages && got.written == pages);
        let first = raw.first().map_or(0, |bytes| device_address(*bytes));
        let contiguous = raw
            .iter()
            .enumerate()
            .all(|(index, bytes)| device_address(*bytes) == first + (index * PAGE) as u64);
        let Ok(address) = u32::try_from(first) else {
            return Status::PinFailed;
        };
        if !complete || !contiguous || u64::from(address) + attach.length > 1 << 32 {
            return Status::PinFailed;
        }
        if let Some(held) = self.buffers.get_mut(slot) {
            *held = Some(Buffer {
                id: attach.buffer,
                _pin: pin,
                address,
                stride: attach.stride,
                width: attach.width,
                height: attach.height,
            });
        }
        Status::Ok
    }

    fn card_bytes(&self) -> u64 {
        self.card.size().unwrap_or(0)
    }

    fn buffer(&self, id: u32) -> Option<&Buffer> {
        self.buffers.iter().flatten().find(|buffer| buffer.id == id)
    }

    /// Point the layer at `rect` of a buffer, from the next frame; buffer 0
    /// takes it off the screen. A rectangle the size of another mode listed
    /// switches to that mode first. A buffer that is not the mode's size, or
    /// a rectangle that does not start it at the screen's corner, is left off
    /// the screen: the core does not wait for an answer to SCANOUT.
    fn scanout(&mut self, id: u32, rect: Rect) -> Result<(), Step> {
        if id == 0 {
            self.ltdc.hide();
            self.shown = None;
            return Ok(());
        }
        let Some(buffer) = self.buffer(id) else {
            return Ok(());
        };
        let start = u64::from(rect.y) * u64::from(buffer.stride) + u64::from(rect.x) * 4;
        let Ok(address) = u32::try_from(u64::from(buffer.address) + start) else {
            return Ok(());
        };
        let frame = Frame {
            address,
            pitch: buffer.stride,
            width: rect.width.min(buffer.width),
            height: rect.height.min(buffer.height),
        };
        let running = self
            .ltdc
            .mode()
            .map(|mode| (u32::from(mode.hdisplay), u32::from(mode.vdisplay)));
        if running != Some((frame.width, frame.height)) {
            let wanted = self
                .runnable
                .as_ref()
                .and_then(|runnable| runnable.of_size(frame.width, frame.height));
            let Some(wanted) = wanted else {
                return Ok(());
            };
            self.switch(&wanted)?;
        }
        if self.ltdc.show(&frame).is_ok() {
            self.shown = Some(id);
        }
        Ok(())
    }

    /// Run another mode: TMDS off, the LTDC stopped, the kernel asked for
    /// the mode's clock, and both started again with the frame's layer off,
    /// as at the start, and the pointer back on its own. Flushes waiting for a reload are answered first, since the
    /// stop is the end of the frame they waited for. A clock the kernel will
    /// not set leaves the mode that ran, and the layer off.
    fn switch(&mut self, wanted: &Candidate) -> Result<(), Step> {
        self.flipped()?;
        let Some(running) = self.ltdc.mode() else {
            return Ok(());
        };
        let _ = self.bridge.disable();
        self.ltdc.stop();
        self.shown = None;
        let mode = if set_clock(&self.device, &wanted.mode) {
            wanted.mode
        } else {
            say(format_args!(
                "ltdc: the kernel would not set the pixel clock for {}; staying at {}",
                Described(&wanted.mode),
                Described(&running)
            ));
            let _ = set_clock(&self.device, &running);
            running
        };
        self.bridge
            .set_mode(&mode, self.hdmi)
            .map_err(|_| Step::BridgeMode)?;
        self.ltdc.start(&mode).map_err(|_| Step::Controller)?;
        // The stop took the pointer off with the frame; it comes back at
        // the same place, clipped to the new screen.
        self.place_pointer();
        self.bridge.enable().map_err(|_| Step::BridgeBus)?;
        say(format_args!("ltdc: now running {}", Described(&mode)));
        Ok(())
    }

    /// Ask for a reload at the next vertical blanking and answer when it
    /// comes; a buffer not on screen has nothing to wait for.
    fn flush(&mut self, buffer: u32, sequence: u64) -> Result<(), Step> {
        self.settle()?;
        if self.shown != Some(buffer) {
            return self.reply(Message::Flipped {
                sequence,
                status: Status::Ok,
            });
        }
        let Some(slot) = self.pending.get_mut(self.waiting) else {
            return Err(Step::Control);
        };
        *slot = sequence;
        self.waiting += 1;
        self.ltdc.request_reload();
        Ok(())
    }

    /// Answer the requests waiting for a reload that has happened already,
    /// whose interrupt the port has yet to deliver: a request made now has
    /// to wait for the next reload, and would otherwise be answered by that
    /// one, a frame early. A pointer being moved keeps the loop reading
    /// MOVEs, so a FLUSH or a CURSOR read in the same batch as the reload is
    /// not rare. The interrupt still comes, and finds nothing to say.
    fn settle(&mut self) -> Result<(), Step> {
        if self.ltdc.take_events().reloaded {
            self.flipped()?;
        }
        Ok(())
    }

    /// CURSOR: show `id`'s image on the second layer with its top-left
    /// corner at `at`, or no pointer for 0, and answer at the reload that
    /// makes it so -- after which the image the layer showed before is no
    /// longer read, and the compositor may draw into it.
    ///
    /// The layer reads the buffer itself: it is one run of memory like any
    /// buffer attached here, and the core cleaned it from the caches before
    /// it sent CURSOR. Nothing is copied, and a compositor that draws each
    /// new image into the buffer the layer is not showing never shows one
    /// half drawn.
    fn cursor(&mut self, id: u32, sequence: u64, at: (i32, i32)) -> Result<(), Step> {
        self.settle()?;
        self.pointer = Pointer {
            buffer: (id != 0).then_some(id),
            at,
        };
        self.place_pointer();
        let Some(slot) = self.pending.get_mut(self.waiting) else {
            return Err(Step::Control);
        };
        *slot = sequence;
        self.waiting += 1;
        self.ltdc.request_reload();
        Ok(())
    }

    /// MOVE: the pointer's image's top-left corner to `at`, from the next
    /// vertical blanking. Nothing waits for it and it waits for nothing: a
    /// flip asked for this frame lands at the same blanking.
    fn move_pointer(&mut self, at: (i32, i32)) {
        self.pointer.at = at;
        if self.pointer.buffer.is_some() {
            self.place_pointer();
        }
    }

    /// Program the second layer from [`Serving::pointer`]: its image where
    /// it is, clipped to the screen, or the layer off.
    fn place_pointer(&mut self) {
        let image = self
            .pointer
            .buffer
            .and_then(|id| self.buffer(id))
            .map(|buffer| CursorImage {
                address: buffer.address,
                pitch: buffer.stride,
                width: buffer.width.min(CURSOR_SIZE),
                height: buffer.height.min(CURSOR_SIZE),
            });
        match image {
            Some(image) if self.ltdc.show_cursor(&image, self.pointer.at).is_ok() => {}
            _ => self.ltdc.hide_cursor(),
        }
    }

    /// Unpin a buffer, once the LTDC has stopped reading it.
    fn detach(&mut self, id: u32) -> Status {
        if self.shown == Some(id) {
            self.ltdc.hide();
            self.shown = None;
        }
        if self.pointer.buffer == Some(id) {
            self.ltdc.hide_cursor();
            self.pointer.buffer = None;
        }
        // A buffer taken off the screen is read until the reload happens.
        if !LEAVE_BUDGET.wait(|| !self.ltdc.reload_pending()) {
            return Status::DeviceRefused;
        }
        match self
            .buffers
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|buffer| buffer.id == id))
        {
            Some(slot) => {
                *slot = None;
                Status::Ok
            }
            None => Status::Invalid,
        }
    }
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

/// A line on standard error, which a native process has open on the
/// console.
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

    fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
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

/// A rate in kHz, as MHz to the kHz.
struct Mhz(u32);

impl fmt::Display for Mhz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03} MHz", self.0 / 1000, self.0 % 1000)
    }
}

/// A mode as a person reads it: size, refresh and pixel clock, and its VIC.
struct Described<'a>(&'a Mode);

impl fmt::Display for Described<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = self.0;
        let millihz = mode.refresh_millihz();
        write!(
            f,
            "{}x{} at {}.{:03} Hz, {}",
            mode.hdisplay,
            mode.vdisplay,
            millihz / 1000,
            millihz % 1000,
            Mhz(mode.clock_khz)
        )?;
        if mode.vic != 0 {
            write!(f, ", VIC {}", mode.vic)?;
        }
        Ok(())
    }
}

/// Say what the monitor is, and its EDID in hex: the one thing a board's
/// serial log has to carry for a monitor to be looked into later.
fn tell_edid(
    base: &[u8; edid::BLOCK_BYTES],
    following: &[[u8; edid::BLOCK_BYTES]],
    extensions: u8,
    hdmi: bool,
) {
    let (name, len) = edid::name(base).unwrap_or(([b'?'; 13], 1));
    let name = core::str::from_utf8(name.get(..len).unwrap_or(&[])).unwrap_or("?");
    let [version, revision] = [18, 19].map(|at| base.get(at).copied().unwrap_or(0));
    say(format_args!(
        "ltdc: monitor {name}, EDID {version}.{revision}, {extensions} extension block{}, {}",
        if extensions == 1 { "" } else { "s" },
        if hdmi { "HDMI" } else { "DVI" }
    ));
    for (number, block) in core::iter::once(base).chain(following).enumerate() {
        for (row, bytes) in block.chunks(32).enumerate() {
            let mut hex = Line::default();
            for byte in bytes {
                let _ = write!(hex, "{byte:02x}");
            }
            say(format_args!(
                "ltdc: edid {:03x} {}",
                number * edid::BLOCK_BYTES + row * 32,
                hex.as_str()
            ));
        }
    }
    match edid::range_limits(base) {
        Some(limits) => say(format_args!(
            "ltdc: range limits {}-{} Hz, {}-{} kHz, {}; takes timings it does not list: {}",
            limits.vertical_hz.0,
            limits.vertical_hz.1,
            limits.horizontal_khz.0,
            limits.horizontal_khz.1,
            Mhz(limits.max_clock_khz),
            if edid::continuous(base) { "yes" } else { "no" }
        )),
        None => say(format_args!("ltdc: no range limits")),
    }
}

/// Say one offered mode, its pixel clock, and whether the board makes it.
fn tell_offer(offer: &Offer, round: &mut impl FnMut(u32) -> Option<u32>) {
    let mut source = Line::default();
    let _ = match offer.source {
        Source::Detailed { block, index } => write!(source, "detailed {block}.{index}"),
        Source::Established(bit) => write!(source, "established bit {bit}"),
        Source::Standard(code) => write!(source, "standard {code:04x}"),
        Source::Vic { vic, native } => {
            write!(source, "VIC {vic}{}", if native { " native" } else { "" })
        }
    };
    let source = source.as_str();
    let mode = match offer.mode {
        Ok(mode) => mode,
        Err(why) => {
            let why = match why {
                Unusable::Interlaced => "interlaced",
                Unusable::Timing => "a timing with analog or composite sync",
                Unusable::Unknown => "no timing known for it",
            };
            if let Source::Standard(code) = offer.source {
                let (w, h, hz) = edid::standard_size(code);
                say(format_args!(
                    "ltdc: offered {source} {w}x{h} at {hz} Hz: {why}"
                ));
            } else {
                say(format_args!("ltdc: offered {source}: {why}"));
            }
            return;
        }
    };
    let described = Described(&mode);
    match choice::verdict(&mode, round) {
        Verdict::Runs { clock_khz } => say(format_args!(
            "ltdc: offered {source} {described}: runs, at {}",
            Mhz(clock_khz)
        )),
        Verdict::TooFast => say(format_args!(
            "ltdc: offered {source} {described}: over the LTDC's {}",
            Mhz(choice::MAX_PIXEL_KHZ)
        )),
        Verdict::TooSlow => say(format_args!(
            "ltdc: offered {source} {described}: under the bridge's {}",
            Mhz(choice::MIN_PIXEL_KHZ)
        )),
        Verdict::Clock { nearest_khz } => say(format_args!(
            "ltdc: offered {source} {described}: the nearest clock is {}",
            Mhz(nearest_khz)
        )),
        Verdict::NoClock => say(format_args!(
            "ltdc: offered {source} {described}: the kernel sets no clock"
        )),
        Verdict::Counters => say(format_args!(
            "ltdc: offered {source} {described}: too large for the LTDC"
        )),
    }
}
