//! A connector's EDID: which file Linux's `drm.edid_firmware=` names for it,
//! whether the file's bytes are an EDID, what the monitor calls itself, and
//! how the card's property calls answer for the blob.
//!
//! `docs/DISPLAY.md` §7 is the design. The short of it: the kernel's display
//! core owns the DRM uAPI, so it is the core that gives a connector its
//! `EDID` property and answers `GETPROPBLOB`, and -- as Linux's DRM core does
//! in `drm_edid_load.c` -- it is the core that reads the file the command line
//! names, from `/lib/firmware`. Nothing here reads a file or a command line;
//! the kernel does, and hands the bytes in.
//!
//! # The grammar, as `drm_load_edid_firmware` reads it
//!
//! `drm.edid_firmware=` is a comma-separated list. An entry `<connector>:<file>`
//! is that connector's; the first whose connector matches is taken. An entry
//! with no colon is every connector's, and the last such is the fallback for a
//! connector no entry names. A connector matches when its name *begins* with
//! the entry's -- Linux compares with `strncmp` over the entry's length, so
//! `DP-1:` is `DP-10`'s too -- and that is kept, since a command line written
//! for Linux has to mean here what it means there. A trailing newline on the
//! name is cut, as there.
//!
//! What is not Linux's: the six built-in EDIDs (`edid/1024x768.bin` and the
//! rest), which Linux has since taken out as well; and the search path,
//! which is `/lib/firmware` alone, with no `updates/` or `firmware_class.path=`.
//!
//! # The bytes, as `edid_load` checks them
//!
//! The file's size must be what its base block says -- 128 bytes for it and
//! for each extension its byte `0x7E` counts. The base block must be valid:
//! at least six of the eight header bytes right (the rest are put right, as
//! `edid_fixup`'s default does), its checksum right, and version 1. An
//! extension block that is not valid is dropped, the blocks after it moved up
//! and the base block's count and checksum made to agree; a CTA-861 block
//! with a wrong checksum is kept, as Linux keeps one.

use ferrix_linux_abi::drm::{self, GetBlob, GetProperty};

/// The command-line option, without its `=`.
pub const PARAMETER: &str = "drm.edid_firmware";

/// Where a named file is looked for: the name is relative to this, as
/// `request_firmware`'s is.
pub const FIRMWARE_DIR: &str = "/lib/firmware/";

/// Bytes of one EDID block.
pub const BLOCK_BYTES: usize = 128;

/// The most an EDID can be: the base block and 255 extensions, which is all
/// byte `0x7E` can count.
pub const MAX_BYTES: usize = 256 * BLOCK_BYTES;

/// The eight bytes every base block starts with.
pub const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// The property's name.
pub const PROPERTY_NAME: &[u8] = b"EDID";

/// The property's flags: `drm_connector_create_standard_properties` makes it
/// an immutable blob.
pub const PROPERTY_FLAGS: u32 = drm::MODE_PROP_BLOB | drm::MODE_PROP_IMMUTABLE;

/// Where the count of extension blocks is in the base block.
const EXTENSIONS_AT: usize = 0x7E;

/// Where a block's checksum byte is.
const CHECKSUM_AT: usize = BLOCK_BYTES - 1;

/// The header bytes that must be right before the rest are put right:
/// `drm_edid.c`'s `edid_fixup` default.
const HEADER_FIXUP: usize = 6;

/// A CTA-861 extension's tag, whose checksum Linux forgives.
const CTA_TAG: u8 = 0x02;

/// The file `setting` -- the value of `drm.edid_firmware=` -- names for the
/// connector called `connector` (`Virtual-1`, `HDMI-A-1`), if any.
#[must_use]
pub fn firmware_for<'a>(setting: &'a str, connector: &str) -> Option<&'a str> {
    let mut fallback = None;
    let mut chosen = None;
    for entry in setting.split(',') {
        match entry.split_once(':') {
            Some((named, file)) => {
                if connector.starts_with(named) {
                    chosen = Some(file);
                    break;
                }
            }
            None if !entry.is_empty() => fallback = Some(entry),
            None => {}
        }
    }
    let name = chosen.or(fallback)?;
    Some(name.strip_suffix('\n').unwrap_or(name))
}

/// Why a name `drm.edid_firmware` gives is not a file under
/// [`FIRMWARE_DIR`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadName {
    /// There is no name.
    Empty,
    /// It begins with `/`, which would name a file anywhere.
    Absolute,
    /// It holds a `..` component, which would climb out.
    Parent,
    /// It holds a NUL, which ends a path early.
    Nul,
}

impl core::fmt::Display for BadName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Empty => "it is empty",
            Self::Absolute => "it is an absolute path",
            Self::Parent => "it climbs out through `..`",
            Self::Nul => "it holds a NUL",
        })
    }
}

/// `name`, if it names a file beneath [`FIRMWARE_DIR`]: not empty, not
/// absolute, with no `..` component and no NUL.
///
/// The command line is trusted, but what the file holds is handed to every
/// program that opens the card, so a name that could reach another file --
/// `../etc/shadow`, `/etc/shadow` -- is refused before anything is read. A
/// symbolic link under `/lib/firmware` could reach one too; the kernel's read
/// refuses to follow any (`fs::read_file_beneath`). Linux's firmware loader
/// is laxer: it joins the name to each of its search directories as it
/// stands.
pub fn confined(name: &str) -> Result<&str, BadName> {
    if name.is_empty() {
        return Err(BadName::Empty);
    }
    if name.starts_with('/') {
        return Err(BadName::Absolute);
    }
    if name.contains('\0') {
        return Err(BadName::Nul);
    }
    if name.split('/').any(|component| component == "..") {
        return Err(BadName::Parent);
    }
    Ok(name)
}

/// Why a file is not an EDID a connector can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unusable {
    /// The file is not the size its base block says it is: `expected` is
    /// that size, 0 for a file too short to have a base block.
    Size {
        /// What the base block says.
        expected: usize,
        /// What the file is.
        found: usize,
    },
    /// The base block is all zeroes.
    Zero,
    /// Fewer than six of the header's eight bytes are right.
    Header,
    /// The base block's checksum is wrong.
    Checksum,
    /// The base block's version is not 1.
    Version(u8),
}

impl core::fmt::Display for Unusable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Size { expected, found } => {
                write!(
                    f,
                    "its size is {found} bytes where its base block says {expected}"
                )
            }
            Self::Zero => f.write_str("its base block is all zeroes"),
            Self::Header => f.write_str("its base block has a corrupt header"),
            Self::Checksum => f.write_str("its base block's checksum is wrong"),
            Self::Version(version) => write!(f, "it is EDID version {version}, not 1"),
        }
    }
}

/// What [`check`] made of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked {
    /// How many of the bytes are the EDID now: the base block and the
    /// extensions that were kept.
    pub len: usize,
    /// How many extensions were kept.
    pub extensions: u8,
    /// How many were dropped as not valid.
    pub dropped: u8,
    /// Whether the header had to be put right.
    pub repaired: bool,
}

/// Check `bytes`, a whole file, as `edid_load` does, putting right what it
/// puts right in place; the EDID is then `bytes[..len]`.
pub fn check(bytes: &mut [u8]) -> Result<Checked, Unusable> {
    let found = bytes.len();
    let expected = match bytes.get(EXTENSIONS_AT) {
        Some(&count) if found >= BLOCK_BYTES => (usize::from(count) + 1) * BLOCK_BYTES,
        _ => 0,
    };
    if expected != found || found == 0 {
        return Err(Unusable::Size { expected, found });
    }
    let (base, rest) = bytes.split_at_mut(BLOCK_BYTES);
    let repaired = check_base(base)?;
    let extensions = rest.len() / BLOCK_BYTES;
    let mut kept = 0usize;
    for index in 0..extensions {
        let at = index * BLOCK_BYTES;
        let valid = rest
            .get(at..at + BLOCK_BYTES)
            .is_some_and(extension_is_valid);
        if !valid {
            continue;
        }
        if kept != index {
            rest.copy_within(at..at + BLOCK_BYTES, kept * BLOCK_BYTES);
        }
        kept += 1;
    }
    let dropped = extensions - kept;
    let kept_count = u8::try_from(kept).unwrap_or(u8::MAX);
    let dropped_count = u8::try_from(dropped).unwrap_or(u8::MAX);
    if dropped > 0 {
        // One less in the count is one more in the checksum, which is how
        // `edid_load` keeps the base block's sum at zero.
        if let Some(sum) = base.get_mut(CHECKSUM_AT) {
            *sum = sum.wrapping_add(dropped_count);
        }
        if let Some(count) = base.get_mut(EXTENSIONS_AT) {
            *count = kept_count;
        }
    }
    Ok(Checked {
        len: (kept + 1) * BLOCK_BYTES,
        extensions: kept_count,
        dropped: dropped_count,
        repaired,
    })
}

/// Check the base block, putting its header right when six or seven of its
/// bytes are; whether that was needed.
fn check_base(base: &mut [u8]) -> Result<bool, Unusable> {
    let right = base
        .iter()
        .zip(HEADER)
        .filter(|&(&byte, want)| byte == want)
        .count();
    if right < HEADER_FIXUP {
        return Err(if is_zero(base) {
            Unusable::Zero
        } else {
            Unusable::Header
        });
    }
    let repaired = right < HEADER.len();
    if let Some(header) = base.get_mut(..HEADER.len()) {
        header.copy_from_slice(&HEADER);
    }
    if !sums_to_zero(base) {
        return Err(if is_zero(base) {
            Unusable::Zero
        } else {
            Unusable::Checksum
        });
    }
    match base.get(18) {
        Some(1) => Ok(repaired),
        Some(&version) => Err(Unusable::Version(version)),
        None => Err(Unusable::Checksum),
    }
}

/// Whether an extension block is one Linux keeps.
fn extension_is_valid(block: &[u8]) -> bool {
    if sums_to_zero(block) {
        return !is_zero(block);
    }
    block.first() == Some(&CTA_TAG)
}

fn sums_to_zero(block: &[u8]) -> bool {
    block.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) == 0
}

fn is_zero(block: &[u8]) -> bool {
    block.iter().all(|&byte| byte == 0)
}

/// Up to thirteen characters of a display descriptor's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Text {
    bytes: [u8; 13],
    len: usize,
}

impl Text {
    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..self.len)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }
}

/// What a monitor says it is, from its base block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The PNP manufacturer id, three capital letters: `LEN`, `DEL`.
    pub manufacturer: [u8; 3],
    /// The product code.
    pub product: u16,
    /// The serial number field, 0 where there is none.
    pub serial_number: u32,
    /// The `0xFC` descriptor: the monitor's name.
    pub name: Option<Text>,
    /// The `0xFF` descriptor: its serial.
    pub serial: Option<Text>,
}

impl Identity {
    /// The manufacturer id as text.
    #[must_use]
    pub fn manufacturer(&self) -> &str {
        core::str::from_utf8(&self.manufacturer).unwrap_or("")
    }
}

/// Where the four 18-byte descriptors start.
const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];

/// Read what a base block says the monitor is: `None` for bytes that are not
/// a base block or whose manufacturer is not three letters.
///
/// The same reading as `src/user/system/linux/compositor/drm`'s `Edid::parse`, which is what a
/// compositor matches `monitor = desc:` against; this one is for the kernel's
/// boot line and for `xtask`, which finds a host's monitor by it.
#[must_use]
pub fn identity(bytes: &[u8]) -> Option<Identity> {
    let base = bytes.get(..BLOCK_BYTES)?;
    if base.get(..HEADER.len())? != HEADER {
        return None;
    }
    let packed = u16::from_be_bytes([*base.get(8)?, *base.get(9)?]);
    let mut manufacturer = [0u8; 3];
    for (letter, shift) in manufacturer.iter_mut().zip([10u16, 5, 0]) {
        let value = u8::try_from((packed >> shift) & 0x1F).ok()?;
        if !(1..=26).contains(&value) {
            return None;
        }
        *letter = b'A' + value - 1;
    }
    Some(Identity {
        manufacturer,
        product: u16::from_le_bytes([*base.get(10)?, *base.get(11)?]),
        serial_number: u32::from_le_bytes([
            *base.get(12)?,
            *base.get(13)?,
            *base.get(14)?,
            *base.get(15)?,
        ]),
        name: descriptor(base, 0xFC),
        serial: descriptor(base, 0xFF),
    })
}

/// The text of the first display descriptor tagged `tag`: up to thirteen
/// bytes, ended by `0x0A`, printable ASCII kept and the spaces round it cut.
fn descriptor(base: &[u8], tag: u8) -> Option<Text> {
    for &at in &DESCRIPTORS {
        let Some(block) = base.get(at..at + 18) else {
            continue;
        };
        if block.get(..3) != Some(&[0, 0, 0]) || block.get(3) != Some(&tag) {
            continue;
        }
        let text = block.get(5..18)?;
        let end = text.iter().position(|&byte| byte == 0x0A).unwrap_or(13);
        let mut out = Text {
            bytes: [0; 13],
            len: 0,
        };
        for &byte in text.get(..end)? {
            if (0x20..0x7F).contains(&byte)
                && let Some(slot) = out.bytes.get_mut(out.len)
            {
                *slot = byte;
                out.len += 1;
            }
        }
        let kept = out.as_str();
        let (start, trimmed) = (kept.len() - kept.trim_start().len(), kept.trim().len());
        if trimmed == 0 {
            continue;
        }
        out.bytes.copy_within(start..start + trimmed, 0);
        out.len = trimmed;
        return Some(out);
    }
    None
}

/// A `DisplayID` extension's tag, which QEMU's EDID carries its preferred
/// timing in when a base block's descriptor cannot hold it.
const DISPLAYID_TAG: u8 = 0x70;

/// A `DisplayID` data block's tag for type I detailed timings.
const DISPLAYID_TYPE_I: u8 = 0x03;

/// Bytes of one `DisplayID` type I timing.
const DISPLAYID_TIMING_BYTES: usize = 20;

/// How many frames a second the monitor's preferred timing shows, in
/// millihertz, as its EDID says: what a virtio-gpu's `GET_EDID` answers
/// with, whose preferred timing QEMU makes at the refresh of the host
/// monitor its window is on (`hw/display/edid-generate.c`).
///
/// The preferred timing is the base block's first detailed timing
/// descriptor -- the first of the four whose pixel clock is not zero, which
/// EDID 1.4 says is the preferred one. A base block whose descriptors are
/// all display descriptors has it in a `DisplayID` extension's first type I
/// timing instead: QEMU puts it there for a mode whose clock or size a
/// descriptor's 16 and 12 bits cannot hold, a large window at 144 Hz among
/// them.
///
/// 0 for anything that is not that: bytes too short for a base block, a
/// header, checksum or version that is not an EDID's, an interlaced timing,
/// a timing with no pixels, or a refresh above
/// [`crate::message::MAX_REFRESH_MHZ`]. Nothing is read past `bytes`, and
/// bytes from a device are read, never trusted: every caller treats 0 as
/// "not known".
#[must_use]
pub fn preferred_refresh_mhz(bytes: &[u8]) -> u32 {
    let Some(base) = bytes.get(..BLOCK_BYTES) else {
        return 0;
    };
    if base.get(..HEADER.len()) != Some(&HEADER[..])
        || !sums_to_zero(base)
        || base.get(18) != Some(&1)
    {
        return 0;
    }
    let found = DESCRIPTORS
        .iter()
        .filter_map(|&at| base.get(at..at + 18))
        .find(|block| {
            block
                .first()
                .zip(block.get(1))
                .is_some_and(|(&low, &high)| low != 0 || high != 0)
        });
    if let Some(block) = found {
        return detailed_refresh(block).unwrap_or(0);
    }
    let extensions = usize::from(base.get(EXTENSIONS_AT).copied().unwrap_or(0));
    (1..=extensions)
        .filter_map(|index| bytes.get(index * BLOCK_BYTES..(index + 1) * BLOCK_BYTES))
        .filter(|block| block.first() == Some(&DISPLAYID_TAG) && sums_to_zero(block))
        .find_map(displayid_timing)
        .and_then(refresh_of)
        .unwrap_or(0)
}

/// A pixel clock in units of 10 kHz, and the active and blanking pixels
/// and lines it is spent on.
#[derive(Clone, Copy)]
struct Spans {
    clock: u32,
    hactive: u32,
    hblank: u32,
    vactive: u32,
    vblank: u32,
}

/// The refresh of an 18-byte detailed timing descriptor: `None` for an
/// interlaced one, or one that does not describe a frame.
fn detailed_refresh(block: &[u8]) -> Option<u32> {
    let byte = |at: usize| block.get(at).copied().map(u32::from);
    // Bit 7 of the flags: interlaced, whose clock paints half a frame.
    if byte(17)? & 0x80 != 0 {
        return None;
    }
    refresh_of(Spans {
        clock: byte(0)? | byte(1)? << 8,
        hactive: byte(2)? | (byte(4)? >> 4) << 8,
        hblank: byte(3)? | (byte(4)? & 0xF) << 8,
        vactive: byte(5)? | (byte(7)? >> 4) << 8,
        vblank: byte(6)? | (byte(7)? & 0xF) << 8,
    })
}

/// A `DisplayID` extension block's first type I timing.
///
/// The section's bytes are the block's from 5, as many as its byte 2 says,
/// and a section that would run into the checksum is not one; each data
/// block in it is a tag, a revision, a length and that many bytes.
fn displayid_timing(block: &[u8]) -> Option<Spans> {
    let end = 5 + usize::from(*block.get(2)?);
    if end > BLOCK_BYTES - 1 {
        return None;
    }
    let mut at = 5;
    while at + 3 <= end {
        let tag = *block.get(at)?;
        let len = usize::from(*block.get(at + 2)?);
        if at + 3 + len > end {
            return None;
        }
        let payload = block.get(at + 3..at + 3 + len)?;
        if tag == DISPLAYID_TYPE_I && len >= DISPLAYID_TIMING_BYTES {
            let byte = |at: usize| payload.get(at).copied().map(u32::from);
            // Every field is stored one less than it is, the clock too.
            let word = |at: usize| Some((byte(at)? | byte(at + 1)? << 8) + 1);
            // Bit 4 of the options: interlaced.
            if byte(3)? & 0x10 != 0 {
                return None;
            }
            return Some(Spans {
                clock: (byte(0)? | byte(1)? << 8 | byte(2)? << 16) + 1,
                hactive: word(4)?,
                hblank: word(6)?,
                vactive: word(12)?,
                vblank: word(14)?,
            });
        }
        at += 3 + len;
    }
    None
}

/// Frames a second in millihertz, rounded: `None` for a frame with no
/// pixels or a rate no monitor has.
fn refresh_of(spans: Spans) -> Option<u32> {
    let total = u64::from(spans.hactive.checked_add(spans.hblank)?)
        * u64::from(spans.vactive.checked_add(spans.vblank)?);
    if spans.clock == 0 || spans.hactive == 0 || spans.vactive == 0 || total == 0 {
        return None;
    }
    // 10 kHz units, to millihertz: 10^4 Hz and 10^3 more.
    let millihertz = (u64::from(spans.clock) * 10_000_000 + total / 2) / total;
    u32::try_from(millihertz)
        .ok()
        .filter(|&rate| (1..=crate::message::MAX_REFRESH_MHZ).contains(&rate))
}

/// `GETPROPERTY` of the `EDID` property, as `drm_mode_getproperty_ioctl`
/// answers a blob property: its name and flags, no values and no enum
/// records, whatever room the caller gave.
pub fn describe_property(property: &mut GetProperty) {
    let mut name = [0u8; drm::PROP_NAME_LEN];
    if let Some(slot) = name.get_mut(..PROPERTY_NAME.len()) {
        slot.copy_from_slice(PROPERTY_NAME);
    }
    property.name = name;
    property.flags = PROPERTY_FLAGS;
    property.count_values = 0;
    property.count_enum_blobs = 0;
}

/// `GETPROPBLOB` of a blob `len` bytes long, as `drm_mode_getblob_ioctl`
/// answers it: the bytes are copied to `data` only when the caller's length
/// is exactly the blob's, and the length written back is the blob's.
/// Whether to copy.
pub fn answer_blob(request: &mut GetBlob, len: usize) -> bool {
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    let copy = request.length == len;
    request.length = len;
    copy
}
