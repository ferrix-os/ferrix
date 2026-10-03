//! The messages, as bytes.
//!
//! Every message starts with its type and its length, four bytes each, and has
//! exactly the length its type fixes. Handles ride in the channel message's
//! handle array; the glue reads their rights and [`Hello::validate`] and
//! [`Ready::HANDLE_RIGHTS`] say what they must be.
//!
//! ```text
//! HELLO     driver -> core, 680 bytes, handles [driver port]
//!   8 version u16   10 scanouts u16   12 location u32
//!   16 scanout 0: width u32, height u32, enabled u32   ... 16 of them, 12 bytes each
//!   208 virgl u16   210 capsets u16   212 capset u32   216 capset_bytes u32
//!   220 cursor u32: 1 for a card with a cursor plane, which CURSOR and MOVE need
//!   224 timings u32: how many of the 16 that follow scanout 0 runs, 0 for any
//!   228 timing 0: clock_khz u32, hdisplay hsync_start hsync_end htotal
//!       vdisplay vsync_start vsync_end vtotal u16 each, flags u32 (bit 0
//!       hsync positive, bit 1 vsync positive)   ... 16 of them, 24 bytes each
//!   612 scanout 0's refresh_mhz u32   ... 16 of them, 4 bytes each
//!   676 flags u32: bit 0 the driver copies pixels with its processor
//! READY     core -> driver, 24 bytes, handles [card VMO, core port]
//!   8 card u32   12 reserved   16 card_bytes u64
//! REFUSED   core -> driver, 12 bytes: 8 reason u32
//! ATTACH    core -> driver, 48 bytes
//!   8 buffer u32   12 format u32   16 offset u64   24 length u64
//!   32 width u32   36 height u32   40 stride u32   44 reserved
//! ATTACH_OBJ core -> driver, 32 bytes
//!   8 buffer u32   12 object u32   16 format u32
//!   20 width u32   24 height u32   28 stride u32
//! ATTACHED  driver -> core, 16 bytes: 8 buffer u32   12 status u32
//! SCANOUT   core -> driver, 32 bytes
//!   8 scanout u32   12 buffer u32 (0: off)   16 rect x, y, width, height u32
//! FLUSH     core -> driver, 40 bytes
//!   8 buffer u32   12 reserved   16 sequence u64   24 rect x, y, width, height u32
//! FLIPPED   driver -> core, 24 bytes: 8 sequence u64   16 status u32   20 reserved
//! DETACH    core -> driver, 16 bytes: 8 buffer u32   12 reserved
//! DETACHED  driver -> core, 16 bytes: 8 buffer u32   12 status u32
//! STOP, STOPPED               8 bytes
//! CURSOR    core -> driver, 40 bytes, answered by FLIPPED
//!   8 scanout u32   12 buffer u32 (0: none)   16 sequence u64
//!   24 hot_x u32   28 hot_y u32   32 x i32   36 y i32
//! MOVE      core -> driver, 24 bytes, not answered
//!   8 scanout u32   12 reserved   16 x i32   20 y i32
//! MODES     driver -> core, 264 bytes, not answered
//!   8 scanout 0: width u32, height u32, enabled u32   ... 16 of them, as HELLO's
//!   200 scanout 0's refresh_mhz u32   ... 16 of them, as HELLO's
//! ```
//!
//! CURSOR shows a [`CURSOR_SIZE`]-square buffer as a scanout's cursor, and
//! is a flush of that buffer in all but name: it takes the next sequence
//! number, waits in the same line, and FLIPPED answers it once the image is
//! on the device. MOVE puts the cursor's top-left corner somewhere else and
//! nobody waits for it, which is the point of a cursor plane: a pointer
//! moving is not a frame.
//!
//! Reserved bytes are written as zero and a message with any of them set is
//! malformed, so they can be given a meaning later without an old reader
//! misreading them.
//!
//! MODES says the device's scanouts have changed since HELLO: a host that
//! resized the window a virtio-gpu is shown in, which the device tells its
//! driver with `VIRTIO_GPU_EVENT_DISPLAY`. It carries each scanout's
//! preferred mode as HELLO did, and replaces what HELLO said; the scanout
//! count is HELLO's and does not change. Nothing answers it, and the core
//! goes on showing whatever it was showing until the card's user asks for
//! another mode.
//!
//! Each scanout's refresh is what its display says it shows a second, in
//! millihertz, and 0 where the driver does not know: a virtio-gpu reads it
//! from the EDID `GET_EDID` returns, whose preferred timing QEMU makes at
//! the host monitor's rate. It follows every other field, so that every
//! offset before it is where it was in version 6 and only the lengths grew;
//! a disabled scanout's is 0, as its size may be, and none is above
//! [`MAX_REFRESH_MHZ`].
//!
//! HELLO's flags (version 8) follow the refreshes, so every earlier offset
//! is still where it was. Bit 0 says the driver shows a buffer by copying
//! it with the processor into memory its device scans out from -- NVIDIA's
//! driver, whose display engine reads its own video memory -- rather than
//! by giving the device the pages. Such a driver is handed the card VMO
//! with `MAP` in place of `TRANSFER` ([`CARD_VMO_COPY_RIGHTS`]), to map it
//! read-only, and only when the kernel finds the device's driver is the
//! one kind that may: the others never are. Every other bit is reserved.
//!
//! HELLO's timings are for a card that runs only the modes it can make a
//! clock for -- a board's HDMI output, not a virtio-gpu, which shows any
//! size and lists none. The first is the mode the scanout runs now and has
//! scanout 0's size; the core lists them all to DRM, the first preferred,
//! and a SCANOUT whose rectangle is another one's size asks the driver to
//! run that one.

use ::core::fmt;

use ferrix_linux_abi::drm::FORMAT_XRGB8888;
use ferrix_native_abi::rights::Rights;

/// The protocol version this crate speaks. 5 added HELLO's timings, 6 MODES,
/// 7 each scanout's refresh, 8 HELLO's flags.
pub const VERSION: u16 = 8;

/// HELLO's type.
pub const HELLO: u32 = 1;
/// READY's type.
pub const READY: u32 = 2;
/// REFUSED's type.
pub const REFUSED: u32 = 3;
/// ATTACH's type.
pub const ATTACH: u32 = 4;
/// ATTACHED's type.
pub const ATTACHED: u32 = 5;
/// SCANOUT's type.
pub const SCANOUT: u32 = 6;
/// FLUSH's type.
pub const FLUSH: u32 = 7;
/// FLIPPED's type.
pub const FLIPPED: u32 = 8;
/// DETACH's type.
pub const DETACH: u32 = 9;
/// DETACHED's type.
pub const DETACHED: u32 = 10;
/// STOP's type.
pub const STOP: u32 = 11;
/// STOPPED's type.
pub const STOPPED: u32 = 12;
/// `ATTACH_OBJ`'s type.
pub const ATTACH_OBJECT: u32 = 13;
/// CURSOR's type.
pub const CURSOR: u32 = 14;
/// MOVE's type.
pub const MOVE: u32 = 15;
/// MODES's type.
pub const MODES: u32 = 16;

/// The width and height of a buffer CURSOR shows: the one cursor size
/// virtio-gpu's host shows.
pub const CURSOR_SIZE: u32 = 64;

/// Bytes of the type and length, and all of STOP and STOPPED.
pub const HEADER_BYTES: usize = 8;
/// The most scanouts a HELLO describes: virtio-gpu's own limit.
pub const MAX_SCANOUTS: usize = 16;
/// Bytes of one scanout in HELLO.
pub const SCANOUT_BYTES: usize = 12;
/// The most timings a HELLO lists.
pub const MAX_TIMINGS: usize = 16;
/// Bytes of one timing in HELLO.
pub const TIMING_BYTES: usize = 24;
/// Where HELLO's timing count lies.
const TIMINGS_AT: usize = 16 + MAX_SCANOUTS * SCANOUT_BYTES + 16;
/// Bytes of one scanout's refresh, which HELLO and MODES carry after
/// everything else.
pub const REFRESH_BYTES: usize = 4;
/// Where HELLO's refreshes lie: after its timings.
const HELLO_REFRESH_AT: usize = TIMINGS_AT + 4 + MAX_TIMINGS * TIMING_BYTES;
/// Where HELLO's flags lie: after its refreshes.
const HELLO_FLAGS_AT: usize = HELLO_REFRESH_AT + MAX_SCANOUTS * REFRESH_BYTES;
/// HELLO's flag for a driver that copies pixels with its processor.
pub const HELLO_COPIES: u32 = 1;
/// Bytes of HELLO.
pub const HELLO_BYTES: usize = HELLO_FLAGS_AT + 4;
/// Where MODES's refreshes lie: after its scanouts.
const MODES_REFRESH_AT: usize = HEADER_BYTES + MAX_SCANOUTS * SCANOUT_BYTES;
/// Bytes of MODES.
pub const MODES_BYTES: usize = MODES_REFRESH_AT + MAX_SCANOUTS * REFRESH_BYTES;
/// Bytes of the longest message.
pub const MAX_BYTES: usize = HELLO_BYTES;

/// The largest width or height a mode may have: `docs/DISPLAY.md` §2.2.
pub const MAX_DIMENSION: u32 = 8192;

/// The fastest refresh a scanout may say it has, in millihertz: 1 kHz, four
/// times the fastest monitor sold, so that a driver's misreading is refused
/// rather than paced to.
pub const MAX_REFRESH_MHZ: u32 = 1_000_000;

/// The most pages one buffer may have: 32 MiB, which holds a 4K mode's
/// `XRGB8888` buffer. A driver sizes its backing lists for this many.
pub const MAX_BUFFER_PAGES: u64 = 8192;

/// The only pixel format iteration 1 attaches: DRM's `XRGB8888`.
pub const FORMAT: u32 = FORMAT_XRGB8888;

/// Bytes per pixel of [`FORMAT`].
pub const BYTES_PER_PIXEL: u32 = 4;

/// The page size buffer ranges are aligned to.
pub const PAGE_SIZE: u64 = 4096;

/// Exactly the rights each side holds the other's port with.
pub const PORT_RIGHTS: Rights = Rights(Rights::WRITE.0 | Rights::TRANSFER.0);

/// Exactly the rights the driver holds the card VMO with: it may read and pin
/// it and was handed it, and may neither write, map nor copy it.
pub const CARD_VMO_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::TRANSFER.0);

/// Exactly the rights a driver whose HELLO says it copies holds the card VMO
/// with: `READ` and `MAP`, so that it can map the buffers read-only and copy
/// from them. Never `WRITE`: it still cannot change a pixel. Nor `TRANSFER`:
/// it maps the card once and closes the handle, and has nobody to pass it
/// to.
pub const CARD_VMO_COPY_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::MAP.0);

/// One scanout as HELLO describes it: its preferred mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScanoutMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether a display is attached.
    pub enabled: bool,
    /// How many frames a second the display shows, in millihertz: 0 when
    /// the driver does not know, and on a scanout with nothing attached.
    pub refresh_mhz: u32,
}

/// One mode a scanout can run, in DRM's terms: each `*_start` and `*_end`
/// counts from the first active pixel or line, and `*total` is the whole
/// period.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Timing {
    /// The pixel clock, in kHz.
    pub clock_khz: u32,
    /// Active pixels a line.
    pub hdisplay: u16,
    /// Where horizontal sync starts.
    pub hsync_start: u16,
    /// Where it ends.
    pub hsync_end: u16,
    /// Pixel clocks a line, blanking included.
    pub htotal: u16,
    /// Active lines a frame.
    pub vdisplay: u16,
    /// Where vertical sync starts.
    pub vsync_start: u16,
    /// Where it ends.
    pub vsync_end: u16,
    /// Lines a frame, blanking included.
    pub vtotal: u16,
    /// Whether horizontal sync is active high.
    pub hsync_high: bool,
    /// Whether vertical sync is active high.
    pub vsync_high: bool,
}

impl Timing {
    /// Whether the numbers describe a mode a display can have: a clock,
    /// every span non-empty and in order, nothing over [`MAX_DIMENSION`].
    #[must_use]
    pub fn is_mode(&self) -> bool {
        let across = [self.hdisplay, self.hsync_start, self.hsync_end, self.htotal];
        let down = [self.vdisplay, self.vsync_start, self.vsync_end, self.vtotal];
        let ordered = |spans: [u16; 4]| {
            spans.first().is_some_and(|&first| first > 0)
                && spans
                    .windows(2)
                    .all(|pair| matches!(pair, [one, two] if one < two))
        };
        self.clock_khz > 0
            && ordered(across)
            && ordered(down)
            && u32::from(self.htotal) <= MAX_DIMENSION * 2
            && u32::from(self.hdisplay) <= MAX_DIMENSION
            && u32::from(self.vdisplay) <= MAX_DIMENSION
    }

    /// Frames a second, rounded.
    #[must_use]
    pub fn refresh_hz(&self) -> u32 {
        let frame = u64::from(self.htotal) * u64::from(self.vtotal);
        if frame == 0 {
            return 0;
        }
        u32::try_from((u64::from(self.clock_khz) * 1000 + frame / 2) / frame).unwrap_or(0)
    }
}

/// The timings a HELLO lists, the first the one running.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Timings {
    /// How many of `list` are timings.
    pub count: u32,
    /// The timings; entries past `count` are zero.
    pub list: [Timing; MAX_TIMINGS],
}

impl Timings {
    /// A card that lists none: a virtual one, which shows any size.
    pub const NONE: Timings = Timings {
        count: 0,
        list: [Timing {
            clock_khz: 0,
            hdisplay: 0,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 0,
            vdisplay: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 0,
            hsync_high: false,
            vsync_high: false,
        }; MAX_TIMINGS],
    };

    /// The timings listed.
    #[must_use]
    pub fn as_slice(&self) -> &[Timing] {
        let count = usize::try_from(self.count).unwrap_or(usize::MAX);
        self.list.get(..count).unwrap_or(&self.list)
    }
}

/// A rectangle in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    /// Left.
    pub x: u32,
    /// Top.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl Rect {
    /// Whether the rectangle is non-empty and lies inside `width` × `height`.
    #[must_use]
    pub fn inside(self, width: u32, height: u32) -> bool {
        self.width != 0
            && self.height != 0
            && self
                .x
                .checked_add(self.width)
                .is_some_and(|end| end <= width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|end| end <= height)
    }
}

/// HELLO: the driver introduces its device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    /// The protocol version.
    pub version: u16,
    /// How many of `modes` describe scanouts.
    pub scanouts: u16,
    /// The device's PCI location, as START gave it.
    pub location: u32,
    /// Each scanout's preferred mode; entries past `scanouts` are zero.
    pub modes: [ScanoutMode; MAX_SCANOUTS],
    /// Whether the device granted `VIRTIO_GPU_F_VIRGL`, which is the whole
    /// of "is there a GPU behind this card".
    ///
    /// Answered by the device, not chosen by the driver: a driver that
    /// asked for 3D on a 2D card gets a no and drives it as a scanout, so
    /// this is what came back rather than what was wanted.
    pub virgl: bool,
    /// How many capability sets the device has, from its configuration
    /// block. Zero on a 2D card, and on a 3D one the count the driver walks
    /// with `GET_CAPSET_INFO`.
    pub capsets: u16,
    /// The first capability set's id, or 0 when there are none:
    /// `gpu::CAPSET_VIRGL2` on a virglrenderer this decade.
    pub capset: u32,
    /// How many bytes of that set the driver actually fetched, which is
    /// what says `GET_CAPSET` ran and not merely `GET_CAPSET_INFO`. Zero
    /// when there was none to fetch, or when it would not fit.
    pub capset_bytes: u32,
    /// Whether the card has a cursor plane, which CURSOR and MOVE need: a
    /// virtio-gpu has its cursor queue, and a card with no such thing says
    /// so rather than be sent what it cannot show.
    pub cursor: bool,
    /// The modes scanout 0 can run, for a card that runs only some: none
    /// for a virtio-gpu, which shows any size.
    pub timings: Timings,
    /// Whether the driver shows a buffer by copying it with the processor,
    /// and so is handed the card VMO it can map ([`HELLO_COPIES`]).
    pub copies: bool,
}

/// Why the core refuses a driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Refusal {
    /// The version is not [`VERSION`].
    Version = 1,
    /// `scanouts` is 0 or above [`MAX_SCANOUTS`].
    Scanouts = 2,
    /// A mode is zero-sized while enabled, above [`MAX_DIMENSION`], or set
    /// past `scanouts`.
    Mode = 3,
    /// A handle is missing, extra, or has rights other than exactly the
    /// specified ones.
    Rights = 4,
    /// The message is not a well-formed HELLO.
    Malformed = 5,
    /// `location` is not the device the channel was made for. Decided by the
    /// glue.
    WrongLocation = 6,
    /// The driver answered something the core did not ask. Decided by
    /// [`crate::session::Session`].
    Protocol = 7,
    /// The firmware's framebuffer is in memory the frame allocator owns
    /// (`docs/DISPLAY.md` §2.4), so no card is published. Decided by the core.
    Framebuffer = 8,
    /// What the driver said about 3D does not hang together: capability
    /// sets on a card that was not granted `VIRTIO_GPU_F_VIRGL`, or a
    /// capability set named where there are none.
    Capsets = 9,
    /// HELLO says the driver copies, and the kernel does not find it one of
    /// the drivers that may: a device whose driver also serves its device
    /// files through the chardev core, which only NVIDIA's does. Decided by
    /// the core.
    Copies = 10,
}

impl Refusal {
    /// The refusal a reason word names, if any.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::Version,
            2 => Self::Scanouts,
            3 => Self::Mode,
            4 => Self::Rights,
            5 => Self::Malformed,
            6 => Self::WrongLocation,
            7 => Self::Protocol,
            8 => Self::Framebuffer,
            9 => Self::Capsets,
            10 => Self::Copies,
            _ => return None,
        })
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Version => "display protocol version mismatch",
            Self::Scanouts => "scanout count out of range",
            Self::Mode => "a scanout mode is not one a display can have",
            Self::Rights => "a handle has the wrong rights",
            Self::Malformed => "malformed HELLO",
            Self::WrongLocation => "HELLO names another device",
            Self::Protocol => "the driver broke the protocol",
            Self::Framebuffer => "the firmware framebuffer is in memory the kernel reclaims",
            Self::Capsets => "HELLO's capability sets do not match what it says about 3D",
            Self::Copies => "HELLO says the driver copies, and this device's driver may not",
        })
    }
}

impl Hello {
    /// HELLO's handles, in order, with exactly the rights each must carry.
    pub const HANDLE_RIGHTS: [Rights; 1] = [PORT_RIGHTS];

    /// Check a HELLO and the rights of the handles it came with.
    pub fn validate(&self, handle_rights: &[Rights]) -> Result<(), Refusal> {
        if self.version != VERSION {
            return Err(Refusal::Version);
        }
        let count = usize::from(self.scanouts);
        if count == 0 || count > MAX_SCANOUTS {
            return Err(Refusal::Scanouts);
        }
        validate_modes(&self.modes, self.scanouts)?;
        self.validate_timings()?;
        // A card that did not get `VIRTIO_GPU_F_VIRGL` has no capability
        // sets to speak of, and one that did and reports none has nothing a
        // 3D driver could use. Either way the pair has to agree, or the
        // driver is saying something it cannot know.
        if !self.virgl && (self.capsets != 0 || self.capset != 0) {
            return Err(Refusal::Capsets);
        }
        if self.capsets == 0 && self.capset != 0 {
            return Err(Refusal::Capsets);
        }
        // Bytes of a set that was never named are bytes of nothing.
        if self.capset == 0 && self.capset_bytes != 0 {
            return Err(Refusal::Capsets);
        }
        if handle_rights != Self::HANDLE_RIGHTS {
            return Err(Refusal::Rights);
        }
        Ok(())
    }

    /// The timings: at most [`MAX_TIMINGS`], each a mode, only on a card of
    /// one scanout, the first scanout 0's own size and that scanout
    /// enabled, and every entry past the count zero.
    fn validate_timings(&self) -> Result<(), Refusal> {
        let count = usize::try_from(self.timings.count).unwrap_or(usize::MAX);
        if count > MAX_TIMINGS {
            return Err(Refusal::Mode);
        }
        let (listed, rest) = self.timings.list.split_at(count);
        if rest.iter().any(|timing| *timing != Timing::default()) {
            return Err(Refusal::Mode);
        }
        let Some(first) = listed.first() else {
            return Ok(());
        };
        let scanout = self.modes.first().copied().unwrap_or_default();
        let fine = self.scanouts == 1
            && scanout.enabled
            && (u32::from(first.hdisplay), u32::from(first.vdisplay))
                == (scanout.width, scanout.height)
            && listed.iter().all(Timing::is_mode);
        if fine { Ok(()) } else { Err(Refusal::Mode) }
    }
}

/// Check a scanout list, HELLO's or MODES's, for a card of `scanouts`: an
/// enabled mode has a size a display can have and a refresh of at most
/// [`MAX_REFRESH_MHZ`], a disabled one no larger and no refresh, and every
/// entry past the count is zero.
///
/// # Errors
///
/// [`Refusal::Mode`] for the first that is not.
pub fn validate_modes(modes: &[ScanoutMode; MAX_SCANOUTS], scanouts: u16) -> Result<(), Refusal> {
    let count = usize::from(scanouts);
    for (index, mode) in modes.iter().enumerate() {
        let fine = if index >= count {
            *mode == ScanoutMode::default()
        } else if mode.enabled {
            (1..=MAX_DIMENSION).contains(&mode.width)
                && (1..=MAX_DIMENSION).contains(&mode.height)
                && mode.refresh_mhz <= MAX_REFRESH_MHZ
        } else {
            mode.width <= MAX_DIMENSION && mode.height <= MAX_DIMENSION && mode.refresh_mhz == 0
        };
        if !fine {
            return Err(Refusal::Mode);
        }
    }
    Ok(())
}

/// READY: the core accepts the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ready {
    /// The card's number, `card<N>`.
    pub card: u32,
    /// Bytes of the card VMO, which every ATTACH's range lies inside.
    pub card_bytes: u64,
}

impl Ready {
    /// READY's handles, in order: the card VMO and the core's port.
    pub const HANDLE_RIGHTS: [Rights; 2] = [CARD_VMO_RIGHTS, Rights::WRITE];
    /// READY's handles for a driver whose HELLO said it copies.
    pub const COPY_HANDLE_RIGHTS: [Rights; 2] = [CARD_VMO_COPY_RIGHTS, Rights::WRITE];

    /// The handles' rights READY carries to a driver whose HELLO said
    /// `copies`.
    #[must_use]
    pub const fn handle_rights(copies: bool) -> [Rights; 2] {
        if copies {
            Self::COPY_HANDLE_RIGHTS
        } else {
            Self::HANDLE_RIGHTS
        }
    }

    /// The card VMO's rights alone, as [`Self::handle_rights`] has them.
    #[must_use]
    pub const fn card_rights(copies: bool) -> Rights {
        if copies {
            CARD_VMO_COPY_RIGHTS
        } else {
            CARD_VMO_RIGHTS
        }
    }
}

/// ATTACH: make a range of the card VMO a buffer the device can show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attach {
    /// The buffer's id, not 0.
    pub buffer: u32,
    /// Its fourcc, [`FORMAT`].
    pub format: u32,
    /// Where its range starts in the card VMO, page-aligned.
    pub offset: u64,
    /// Its range's length, whole pages.
    pub length: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
}

/// `ATTACH_OBJ`: make an object the *render* conversation created a buffer
/// the device can show.
///
/// The other way to make a buffer, and the reason there are two: an ATTACH's
/// pixels are guest memory the device is given, and these pixels are on the
/// GPU already. A compositor that drew its frame there would otherwise have
/// to fetch it and hand it back, which is the whole picture crossing between
/// host and guest twice a frame.
///
/// What the driver is given is the *device's* own name for the resource --
/// `docs/GPU.md` §3.3's rule, that what crosses this seam untouched is the
/// driver's language -- and the core knows it only as a number it was told
/// by the render core, which hands out object ids from a range no display
/// buffer uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttachObject {
    /// The buffer's id, not 0.
    pub buffer: u32,
    /// The renderer's object, not 0.
    pub object: u32,
    /// Its fourcc, [`FORMAT`].
    pub format: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
}

impl AttachObject {
    /// Check the buffer against itself.
    ///
    /// No range to check: the pixels are the device's own and the core never
    /// says where they are.
    ///
    /// # Errors
    ///
    /// Why it is not one a driver can act on.
    pub fn validate(&self) -> Result<(), AttachError> {
        if self.buffer == 0 || self.object == 0 {
            return Err(AttachError::Id);
        }
        if self.format != FORMAT {
            return Err(AttachError::Format);
        }
        if !(1..=MAX_DIMENSION).contains(&self.width) || !(1..=MAX_DIMENSION).contains(&self.height)
        {
            return Err(AttachError::Size);
        }
        if u64::from(self.stride) < u64::from(self.width) * u64::from(BYTES_PER_PIXEL) {
            return Err(AttachError::Stride);
        }
        Ok(())
    }
}

/// Why an ATTACH is not one a driver can act on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttachError {
    /// Buffer id 0.
    Id,
    /// A format other than [`FORMAT`].
    Format,
    /// A size of 0 or above [`MAX_DIMENSION`].
    Size,
    /// A stride under `width` × 4, or rows that do not fit the range.
    Stride,
    /// A range that is not whole pages, is empty, is longer than
    /// [`MAX_BUFFER_PAGES`], or runs past the card VMO.
    Range,
}

impl Attach {
    /// Check the buffer against itself and a card VMO of `card_bytes`.
    pub fn validate(&self, card_bytes: u64) -> Result<(), AttachError> {
        if self.buffer == 0 {
            return Err(AttachError::Id);
        }
        if self.format != FORMAT {
            return Err(AttachError::Format);
        }
        if !(1..=MAX_DIMENSION).contains(&self.width) || !(1..=MAX_DIMENSION).contains(&self.height)
        {
            return Err(AttachError::Size);
        }
        let row = u64::from(self.width) * u64::from(BYTES_PER_PIXEL);
        let pixels = u64::from(self.stride) * u64::from(self.height);
        if u64::from(self.stride) < row || pixels > self.length {
            return Err(AttachError::Stride);
        }
        let end = self.offset.checked_add(self.length);
        if self.length == 0
            || self.length > MAX_BUFFER_PAGES * PAGE_SIZE
            || !self.offset.is_multiple_of(PAGE_SIZE)
            || !self.length.is_multiple_of(PAGE_SIZE)
            || end.is_none_or(|end| end > card_bytes)
        {
            return Err(AttachError::Range);
        }
        Ok(())
    }
}

/// The status a driver reports for ATTACH, FLUSH or DETACH.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Status {
    /// Done.
    Ok = 0,
    /// The device refused the command.
    DeviceRefused = 1,
    /// Pinning the range failed.
    PinFailed = 2,
    /// The driver or device ran out of memory.
    OutOfMemory = 3,
    /// The request was not one the driver could act on.
    Invalid = 4,
}

impl Status {
    /// The status a word names, if any.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::Ok,
            1 => Self::DeviceRefused,
            2 => Self::PinFailed,
            3 => Self::OutOfMemory,
            4 => Self::Invalid,
            _ => return None,
        })
    }
}

/// Every message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "HELLO's timings make it 600 bytes; a message lives on a stack for one decode, \
              this crate has no allocator to box it in, and 600 bytes copied per message is \
              nothing beside the channel round trip that carried it"
)]
pub enum Message {
    /// Driver to core.
    Hello(Hello),
    /// Core to driver.
    Ready(Ready),
    /// Core to driver.
    Refused(Refusal),
    /// Core to driver.
    Attach(Attach),
    /// Core to driver: a buffer whose pixels are already on the device.
    AttachObject(AttachObject),
    /// Driver to core.
    Attached {
        /// The buffer.
        buffer: u32,
        /// How it went.
        status: Status,
    },
    /// Core to driver.
    Scanout {
        /// The scanout.
        scanout: u32,
        /// The buffer, or 0 to turn the scanout off.
        buffer: u32,
        /// The part of the buffer to show.
        rect: Rect,
    },
    /// Core to driver.
    Flush {
        /// The buffer.
        buffer: u32,
        /// The flush's number, counting from 1 per driver.
        sequence: u64,
        /// What changed.
        rect: Rect,
    },
    /// Driver to core.
    Flipped {
        /// The flush that finished.
        sequence: u64,
        /// How it went.
        status: Status,
    },
    /// Core to driver.
    Detach {
        /// The buffer.
        buffer: u32,
    },
    /// Driver to core.
    Detached {
        /// The buffer.
        buffer: u32,
        /// How it went.
        status: Status,
    },
    /// Core to driver.
    Stop,
    /// Driver to core.
    Stopped,
    /// Core to driver: `buffer` is `scanout`'s cursor, with its top-left
    /// corner at (`x`, `y`). FLIPPED answers it, by `sequence`.
    Cursor {
        /// The scanout.
        scanout: u32,
        /// A [`CURSOR_SIZE`]-square buffer, or 0 for none.
        buffer: u32,
        /// Its number, from the flushes' count.
        sequence: u64,
        /// The hotspot, from the image's left edge; below [`CURSOR_SIZE`].
        hot_x: u32,
        /// The hotspot, from the image's top edge; below [`CURSOR_SIZE`].
        hot_y: u32,
        /// The image's left edge on the scanout.
        x: i32,
        /// The image's top edge on the scanout.
        y: i32,
    },
    /// Core to driver: `scanout`'s cursor's top-left corner is at (`x`,
    /// `y`) now. Not answered.
    Move {
        /// The scanout.
        scanout: u32,
        /// The image's left edge on the scanout.
        x: i32,
        /// The image's top edge on the scanout.
        y: i32,
    },
    /// Driver to core: each scanout's preferred mode now, in HELLO's place.
    /// Not answered.
    Modes {
        /// Each scanout's preferred mode; entries past HELLO's count are
        /// zero.
        modes: [ScanoutMode; MAX_SCANOUTS],
    },
}

/// Why bytes are not a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageError {
    /// Shorter than a header.
    Short,
    /// A type this crate does not know.
    Type(u32),
    /// A length other than the type's, or other than the bytes given.
    Length,
    /// A field outside its range, or a reserved byte set.
    Field,
}

/// An encoded message: bytes up to [`MAX_BYTES`] and how many are used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    bytes: [u8; MAX_BYTES],
    len: usize,
}

impl Encoded {
    /// The message's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl Message {
    /// The message's type.
    #[must_use]
    pub const fn kind(&self) -> u32 {
        match self {
            Self::Hello(_) => HELLO,
            Self::Ready(_) => READY,
            Self::Refused(_) => REFUSED,
            Self::Attach(_) => ATTACH,
            Self::AttachObject(_) => ATTACH_OBJECT,
            Self::Attached { .. } => ATTACHED,
            Self::Scanout { .. } => SCANOUT,
            Self::Flush { .. } => FLUSH,
            Self::Flipped { .. } => FLIPPED,
            Self::Detach { .. } => DETACH,
            Self::Detached { .. } => DETACHED,
            Self::Stop => STOP,
            Self::Stopped => STOPPED,
            Self::Cursor { .. } => CURSOR,
            Self::Move { .. } => MOVE,
            Self::Modes { .. } => MODES,
        }
    }

    /// The fixed length of a message of type `kind`.
    #[must_use]
    pub const fn length_of(kind: u32) -> Option<usize> {
        Some(match kind {
            HELLO => HELLO_BYTES,
            READY => 24,
            REFUSED => 12,
            ATTACH => 48,
            ATTACH_OBJECT => 32,
            ATTACHED | DETACH | DETACHED => 16,
            SCANOUT => 32,
            FLUSH | CURSOR => 40,
            FLIPPED | MOVE => 24,
            STOP | STOPPED => HEADER_BYTES,
            MODES => MODES_BYTES,
            _ => return None,
        })
    }

    /// Encode the message.
    #[must_use]
    pub fn encode(&self) -> Encoded {
        let kind = self.kind();
        let len = Self::length_of(kind).unwrap_or(HEADER_BYTES);
        let mut out = Encoded {
            bytes: [0; MAX_BYTES],
            len,
        };
        let bytes = &mut out.bytes;
        put32(bytes, 0, kind);
        put32(bytes, 4, u32::try_from(len).unwrap_or(0));
        match *self {
            Self::Hello(hello) => put_hello(bytes, &hello),
            Self::Ready(ready) => {
                put32(bytes, 8, ready.card);
                put64(bytes, 16, ready.card_bytes);
            }
            Self::Refused(reason) => put32(bytes, 8, reason as u32),
            Self::Attach(attach) => {
                put32(bytes, 8, attach.buffer);
                put32(bytes, 12, attach.format);
                put64(bytes, 16, attach.offset);
                put64(bytes, 24, attach.length);
                put32(bytes, 32, attach.width);
                put32(bytes, 36, attach.height);
                put32(bytes, 40, attach.stride);
            }
            Self::AttachObject(attach) => {
                put32(bytes, 8, attach.buffer);
                put32(bytes, 12, attach.object);
                put32(bytes, 16, attach.format);
                put32(bytes, 20, attach.width);
                put32(bytes, 24, attach.height);
                put32(bytes, 28, attach.stride);
            }
            Self::Attached { buffer, status } | Self::Detached { buffer, status } => {
                put32(bytes, 8, buffer);
                put32(bytes, 12, status as u32);
            }
            Self::Scanout {
                scanout,
                buffer,
                rect,
            } => {
                put32(bytes, 8, scanout);
                put32(bytes, 12, buffer);
                put_rect(bytes, 16, rect);
            }
            Self::Flush {
                buffer,
                sequence,
                rect,
            } => {
                put32(bytes, 8, buffer);
                put64(bytes, 16, sequence);
                put_rect(bytes, 24, rect);
            }
            Self::Flipped { sequence, status } => {
                put64(bytes, 8, sequence);
                put32(bytes, 16, status as u32);
            }
            Self::Detach { buffer } => put32(bytes, 8, buffer),
            Self::Stop | Self::Stopped => {}
            Self::Cursor {
                scanout,
                buffer,
                sequence,
                hot_x,
                hot_y,
                x,
                y,
            } => {
                put32(bytes, 8, scanout);
                put32(bytes, 12, buffer);
                put64(bytes, 16, sequence);
                put32(bytes, 24, hot_x);
                put32(bytes, 28, hot_y);
                put32(bytes, 32, x as u32);
                put32(bytes, 36, y as u32);
            }
            Self::Move { scanout, x, y } => {
                put32(bytes, 8, scanout);
                put32(bytes, 16, x as u32);
                put32(bytes, 20, y as u32);
            }
            Self::Modes { modes } => {
                put_modes(bytes, HEADER_BYTES, &modes);
                put_refreshes(bytes, MODES_REFRESH_AT, &modes);
            }
        }
        out
    }

    /// Decode a message, refusing anything but exactly one well-formed one.
    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        let kind = get32(bytes, 0).ok_or(MessageError::Short)?;
        let length = get32(bytes, 4).ok_or(MessageError::Short)? as usize;
        let expected = Self::length_of(kind).ok_or(MessageError::Type(kind))?;
        if length != expected || bytes.len() != expected {
            return Err(MessageError::Length);
        }
        decode_body(kind, bytes).ok_or(MessageError::Field)
    }
}

fn decode_body(kind: u32, bytes: &[u8]) -> Option<Message> {
    let zero32 = |at: usize| get32(bytes, at).filter(|&value| value == 0).map(drop);
    let status = |at: usize| get32(bytes, at).and_then(Status::from_raw);
    Some(match kind {
        HELLO => decode_hello(bytes)?,
        READY => {
            zero32(12)?;
            Message::Ready(Ready {
                card: get32(bytes, 8)?,
                card_bytes: get64(bytes, 16)?,
            })
        }
        REFUSED => Message::Refused(Refusal::from_raw(get32(bytes, 8)?)?),
        ATTACH_OBJECT => Message::AttachObject(AttachObject {
            buffer: get32(bytes, 8)?,
            object: get32(bytes, 12)?,
            format: get32(bytes, 16)?,
            width: get32(bytes, 20)?,
            height: get32(bytes, 24)?,
            stride: get32(bytes, 28)?,
        }),
        ATTACH => {
            zero32(44)?;
            Message::Attach(Attach {
                buffer: get32(bytes, 8)?,
                format: get32(bytes, 12)?,
                offset: get64(bytes, 16)?,
                length: get64(bytes, 24)?,
                width: get32(bytes, 32)?,
                height: get32(bytes, 36)?,
                stride: get32(bytes, 40)?,
            })
        }
        ATTACHED => Message::Attached {
            buffer: get32(bytes, 8)?,
            status: status(12)?,
        },
        DETACHED => Message::Detached {
            buffer: get32(bytes, 8)?,
            status: status(12)?,
        },
        SCANOUT => Message::Scanout {
            scanout: get32(bytes, 8)?,
            buffer: get32(bytes, 12)?,
            rect: get_rect(bytes, 16)?,
        },
        FLUSH => {
            zero32(12)?;
            Message::Flush {
                buffer: get32(bytes, 8)?,
                sequence: get64(bytes, 16)?,
                rect: get_rect(bytes, 24)?,
            }
        }
        FLIPPED => {
            zero32(20)?;
            Message::Flipped {
                sequence: get64(bytes, 8)?,
                status: status(16)?,
            }
        }
        DETACH => {
            zero32(12)?;
            Message::Detach {
                buffer: get32(bytes, 8)?,
            }
        }
        STOP => Message::Stop,
        STOPPED => Message::Stopped,
        CURSOR | MOVE => return decode_cursor(kind, bytes),
        MODES => Message::Modes {
            modes: get_refreshes(bytes, MODES_REFRESH_AT, get_modes(bytes, HEADER_BYTES)?)?,
        },
        _ => return None,
    })
}

/// [`decode_body`] for the cursor's two messages.
fn decode_cursor(kind: u32, bytes: &[u8]) -> Option<Message> {
    let signed = |at: usize| get32(bytes, at).map(|value| value as i32);
    Some(match kind {
        CURSOR => {
            let hot = |at: usize| get32(bytes, at).filter(|&hot| hot < CURSOR_SIZE);
            Message::Cursor {
                scanout: get32(bytes, 8)?,
                buffer: get32(bytes, 12)?,
                sequence: get64(bytes, 16)?,
                hot_x: hot(24)?,
                hot_y: hot(28)?,
                x: signed(32)?,
                y: signed(36)?,
            }
        }
        MOVE => {
            let _reserved = get32(bytes, 12).filter(|&reserved| reserved == 0)?;
            Message::Move {
                scanout: get32(bytes, 8)?,
                x: signed(16)?,
                y: signed(20)?,
            }
        }
        _ => return None,
    })
}

/// HELLO's fields, into `bytes`.
fn put_hello(bytes: &mut [u8], hello: &Hello) {
    put16(bytes, 8, hello.version);
    put16(bytes, 10, hello.scanouts);
    put32(bytes, 12, hello.location);
    put_modes(bytes, 16, &hello.modes);
    // After the modes, so that every offset above is where it
    // has always been and only the length grew.
    let at = 16 + MAX_SCANOUTS * SCANOUT_BYTES;
    put16(bytes, at, u16::from(hello.virgl));
    put16(bytes, at + 2, hello.capsets);
    put32(bytes, at + 4, hello.capset);
    put32(bytes, at + 8, hello.capset_bytes);
    put32(bytes, at + 12, u32::from(hello.cursor));
    put32(bytes, TIMINGS_AT, hello.timings.count);
    for (index, timing) in hello.timings.list.iter().enumerate() {
        let at = TIMINGS_AT + 4 + index * TIMING_BYTES;
        put32(bytes, at, timing.clock_khz);
        let spans = [
            timing.hdisplay,
            timing.hsync_start,
            timing.hsync_end,
            timing.htotal,
            timing.vdisplay,
            timing.vsync_start,
            timing.vsync_end,
            timing.vtotal,
        ];
        for (slot, span) in spans.into_iter().enumerate() {
            put16(bytes, at + 4 + slot * 2, span);
        }
        let flags = u32::from(timing.hsync_high) | (u32::from(timing.vsync_high) << 1);
        put32(bytes, at + 20, flags);
    }
    put_refreshes(bytes, HELLO_REFRESH_AT, &hello.modes);
    put32(
        bytes,
        HELLO_FLAGS_AT,
        if hello.copies { HELLO_COPIES } else { 0 },
    );
}

/// HELLO's timing at `index`: `None` for flags other than the two sync
/// polarities.
fn get_timing(bytes: &[u8], index: usize) -> Option<Timing> {
    let at = TIMINGS_AT + 4 + index * TIMING_BYTES;
    let span = |slot: usize| get16(bytes, at + 4 + slot * 2);
    let flags = get32(bytes, at + 20).filter(|&flags| flags & !0x3 == 0)?;
    Some(Timing {
        clock_khz: get32(bytes, at)?,
        hdisplay: span(0)?,
        hsync_start: span(1)?,
        hsync_end: span(2)?,
        htotal: span(3)?,
        vdisplay: span(4)?,
        vsync_start: span(5)?,
        vsync_end: span(6)?,
        vtotal: span(7)?,
        hsync_high: flags & 1 != 0,
        vsync_high: flags & 2 != 0,
    })
}

/// HELLO, from `bytes`: `None` for a field outside its range.
fn decode_hello(bytes: &[u8]) -> Option<Message> {
    let modes = get_refreshes(bytes, HELLO_REFRESH_AT, get_modes(bytes, 16)?)?;
    let mut timings = Timings {
        count: get32(bytes, TIMINGS_AT)?,
        ..Timings::NONE
    };
    for (index, timing) in timings.list.iter_mut().enumerate() {
        *timing = get_timing(bytes, index)?;
    }
    let at = 16 + MAX_SCANOUTS * SCANOUT_BYTES;
    Some(Message::Hello(Hello {
        version: get16(bytes, 8)?,
        scanouts: get16(bytes, 10)?,
        location: get32(bytes, 12)?,
        modes,
        virgl: match get16(bytes, at)? {
            0 => false,
            1 => true,
            _ => return None,
        },
        capsets: get16(bytes, at + 2)?,
        capset: get32(bytes, at + 4)?,
        capset_bytes: get32(bytes, at + 8)?,
        cursor: match get32(bytes, at + 12)? {
            0 => false,
            1 => true,
            _ => return None,
        },
        timings,
        // Every bit but the one is reserved, and a reserved bit set is a
        // malformed HELLO.
        copies: match get32(bytes, HELLO_FLAGS_AT)? {
            0 => false,
            HELLO_COPIES => true,
            _ => return None,
        },
    }))
}

/// The [`MAX_SCANOUTS`] scanouts HELLO and MODES carry, starting at `at`.
fn put_modes(bytes: &mut [u8], at: usize, modes: &[ScanoutMode; MAX_SCANOUTS]) {
    for (index, mode) in modes.iter().enumerate() {
        let at = at + index * SCANOUT_BYTES;
        put32(bytes, at, mode.width);
        put32(bytes, at + 4, mode.height);
        put32(bytes, at + 8, u32::from(mode.enabled));
    }
}

/// [`put_modes`]'s scanouts back: `None` for an `enabled` other than 0 or 1.
fn get_modes(bytes: &[u8], at: usize) -> Option<[ScanoutMode; MAX_SCANOUTS]> {
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    for (index, mode) in modes.iter_mut().enumerate() {
        let at = at + index * SCANOUT_BYTES;
        *mode = ScanoutMode {
            width: get32(bytes, at)?,
            height: get32(bytes, at + 4)?,
            enabled: match get32(bytes, at + 8)? {
                0 => false,
                1 => true,
                _ => return None,
            },
            refresh_mhz: 0,
        };
    }
    Some(modes)
}

/// Each scanout's refresh, which HELLO and MODES carry after everything
/// else, from `at` on.
fn put_refreshes(bytes: &mut [u8], at: usize, modes: &[ScanoutMode; MAX_SCANOUTS]) {
    for (index, mode) in modes.iter().enumerate() {
        put32(bytes, at + index * REFRESH_BYTES, mode.refresh_mhz);
    }
}

/// [`put_refreshes`]'s refreshes back, into `modes`.
fn get_refreshes(
    bytes: &[u8],
    at: usize,
    mut modes: [ScanoutMode; MAX_SCANOUTS],
) -> Option<[ScanoutMode; MAX_SCANOUTS]> {
    for (index, mode) in modes.iter_mut().enumerate() {
        mode.refresh_mhz = get32(bytes, at + index * REFRESH_BYTES)?;
    }
    Some(modes)
}

fn get16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn get32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn get64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn get_rect(bytes: &[u8], at: usize) -> Option<Rect> {
    Some(Rect {
        x: get32(bytes, at)?,
        y: get32(bytes, at + 4)?,
        width: get32(bytes, at + 8)?,
        height: get32(bytes, at + 12)?,
    })
}

fn put(out: &mut [u8], at: usize, field: &[u8]) {
    if let Some(slot) = at
        .checked_add(field.len())
        .and_then(|end| out.get_mut(at..end))
    {
        slot.copy_from_slice(field);
    }
}

fn put16(out: &mut [u8], at: usize, value: u16) {
    put(out, at, &value.to_le_bytes());
}

fn put32(out: &mut [u8], at: usize, value: u32) {
    put(out, at, &value.to_le_bytes());
}

fn put64(out: &mut [u8], at: usize, value: u64) {
    put(out, at, &value.to_le_bytes());
}

fn put_rect(out: &mut [u8], at: usize, rect: Rect) {
    put32(out, at, rect.x);
    put32(out, at + 4, rect.y);
    put32(out, at + 8, rect.width);
    put32(out, at + 12, rect.height);
}
