//! A driver's requirement table, and devmgr's check of it against a generic
//! device-tree node before the driver is started (`docs/DRIVER-MODEL.md`
//! §3 and §8).
//!
//! A driver of a generic node says what it needs from the node -- register
//! ranges, interrupts, a clock by its tree name, a pin state, a supply --
//! in a table embedded in its image, in the section [`SECTION`].
//! `ferrix-driver`'s `#[derive(Resources)]` writes it with [`encode`]; a
//! driver may write one by hand the same way. devmgr reads it out of the
//! image with [`find_section`] and holds the node's identity record to it
//! with [`check`] before it starts anything, so a node that lacks a clock
//! its driver needs is refused at admission, with the node and the
//! reference named, never in the driver's own bind.
//!
//! # Layout
//!
//! Little-endian, at most [`MAX_BYTES`]:
//!
//! ```text
//! 0 magic u32 (MAGIC)   4 version u16 (1)   6 count u16
//! then each requirement, four-byte aligned:
//! 0 what u8   1 flags u8   2 index u16 (BY_NAME when named)
//! 4 name length u8   5 0 0 0   8 name, padded to four
//! ```

use core::fmt;

use ferrix_fdt::identity::{Record, RecordReference};

/// The section of a driver image the table is in.
pub const SECTION: &str = ".ferrix.requirements";

/// The table's first word: `FXRQ` in memory order.
pub const MAGIC: u32 = u32::from_le_bytes(*b"FXRQ");

/// The version this crate writes and reads.
pub const VERSION: u16 = 1;

/// The most bytes a table takes.
pub const MAX_BYTES: usize = 1024;

/// The most requirements one table holds.
pub const MAX_REQUIREMENTS: usize = 32;

/// [`Requirement::index`]'s value for a requirement named, not numbered.
pub const BY_NAME: u16 = u16::MAX;

/// Requirement flag: the driver runs without it.
pub const OPTIONAL: u8 = 1 << 0;

/// What a requirement asks of the node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum What {
    /// A `reg` range, as an aperture; named by `reg-names`.
    Registers = 1,
    /// An interrupt, as a vector; named by `interrupt-names`.
    Interrupt = 2,
    /// A `clocks` reference; named by `clock-names`.
    Clock = 3,
    /// A `resets` reference; named by `reset-names`.
    Reset = 4,
    /// A pin state: `pinctrl-<n>`, named by `pinctrl-names`.
    PinControl = 5,
    /// A `<name>-supply` reference.
    PowerSupply = 6,
    /// The channel to a parent bus's driver: only a child device has one.
    ParentBus = 7,
}

impl What {
    /// The kind numbered `value`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<What> {
        Some(match value {
            1 => What::Registers,
            2 => What::Interrupt,
            3 => What::Clock,
            4 => What::Reset,
            5 => What::PinControl,
            6 => What::PowerSupply,
            7 => What::ParentBus,
            _ => return None,
        })
    }

    /// What it is called in a refusal's line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            What::Registers => "registers",
            What::Interrupt => "interrupt",
            What::Clock => "clock",
            What::Reset => "reset",
            What::PinControl => "pin control state",
            What::PowerSupply => "power supply",
            What::ParentBus => "parent bus",
        }
    }

    /// The tree's reference property and the names property beside it.
    const fn properties(self) -> (&'static str, &'static str) {
        match self {
            What::Registers => ("reg", "reg-names"),
            What::Interrupt => ("interrupts", "interrupt-names"),
            What::Clock => ("clocks", "clock-names"),
            What::Reset => ("resets", "reset-names"),
            What::PinControl => ("pinctrl-", "pinctrl-names"),
            What::PowerSupply => ("-supply", ""),
            What::ParentBus => ("", ""),
        }
    }
}

/// One requirement of a driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Requirement<'a> {
    /// What it asks for.
    pub what: What,
    /// Its place among its kind, or [`BY_NAME`].
    pub index: u16,
    /// Its tree name, when named: `apb_pclk`, `default`, `vs`.
    pub name: &'a str,
    /// Whether the driver runs without it.
    pub optional: bool,
}

impl<'a> Requirement<'a> {
    /// A requirement by place.
    #[must_use]
    pub const fn at(what: What, index: u16) -> Self {
        Requirement {
            what,
            index,
            name: "",
            optional: false,
        }
    }

    /// A requirement by tree name.
    #[must_use]
    pub const fn named(what: What, name: &'a str) -> Self {
        Requirement {
            what,
            index: BY_NAME,
            name,
            optional: false,
        }
    }

    /// The same, the driver running without it.
    #[must_use]
    pub const fn optional(self) -> Self {
        Requirement {
            optional: true,
            ..self
        }
    }
}

/// The bytes [`encode`] writes for `requirements`: what a table's array
/// must be sized to.
#[must_use]
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: evaluated at compile time, so an index out of bounds fails the build, never a run"
)]
pub const fn size(requirements: &[Requirement<'_>]) -> usize {
    let mut bytes = 8;
    let mut index = 0;
    while index < requirements.len() {
        bytes += 8 + requirements[index].name.len().next_multiple_of(4);
        index += 1;
    }
    bytes
}

/// The table for `requirements`, at compile time: `N` is [`size`]'s. A
/// table too long, a name over 255 bytes, or the wrong `N`, fails the
/// build.
#[must_use]
#[expect(
    clippy::indexing_slicing,
    reason = "AUDIT: evaluated at compile time, so an index out of bounds fails the build, never a run"
)]
pub const fn encode<const N: usize>(requirements: &[Requirement<'_>]) -> [u8; N] {
    assert!(N == size(requirements), "the table's size is size()'s");
    assert!(N <= MAX_BYTES, "a requirement table is at most MAX_BYTES");
    assert!(
        requirements.len() <= MAX_REQUIREMENTS,
        "at most MAX_REQUIREMENTS"
    );
    let mut out = [0u8; N];
    let magic = MAGIC.to_le_bytes();
    let version = VERSION.to_le_bytes();
    let count = (requirements.len() as u16).to_le_bytes();
    out[0] = magic[0];
    out[1] = magic[1];
    out[2] = magic[2];
    out[3] = magic[3];
    out[4] = version[0];
    out[5] = version[1];
    out[6] = count[0];
    out[7] = count[1];
    let mut at = 8;
    let mut index = 0;
    while index < requirements.len() {
        let requirement = requirements[index];
        let name = requirement.name.as_bytes();
        assert!(name.len() <= 255, "a tree name is at most 255 bytes");
        out[at] = requirement.what as u8;
        out[at + 1] = if requirement.optional { OPTIONAL } else { 0 };
        let place = requirement.index.to_le_bytes();
        out[at + 2] = place[0];
        out[at + 3] = place[1];
        out[at + 4] = name.len() as u8;
        let mut byte = 0;
        while byte < name.len() {
            out[at + 8 + byte] = name[byte];
            byte += 1;
        }
        at += 8 + name.len().next_multiple_of(4);
        index += 1;
    }
    out
}

/// Why bytes are not a requirement table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Not [`MAGIC`] and [`VERSION`], or longer than [`MAX_BYTES`].
    NotATable,
    /// A requirement runs past the end, or names a kind this crate does
    /// not know.
    Entry,
}

/// A requirement table, read back.
#[derive(Clone, Copy, Debug)]
pub struct Table<'t> {
    bytes: &'t [u8],
    count: u16,
}

impl<'t> Table<'t> {
    /// Read the table at the start of `bytes`, checking every entry.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn parse(bytes: &'t [u8]) -> Result<Self, Malformed> {
        if bytes.len() > MAX_BYTES || bytes.get(..4) != Some(&MAGIC.to_le_bytes()[..]) {
            return Err(Malformed::NotATable);
        }
        if bytes.get(4..6) != Some(&VERSION.to_le_bytes()[..]) {
            return Err(Malformed::NotATable);
        }
        let count = bytes
            .get(6..8)
            .and_then(|word| word.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or(Malformed::NotATable)?;
        if usize::from(count) > MAX_REQUIREMENTS {
            return Err(Malformed::NotATable);
        }
        let table = Table { bytes, count };
        if table.iter().count() != usize::from(count) {
            return Err(Malformed::Entry);
        }
        Ok(table)
    }

    /// Every requirement, in order.
    pub fn iter(&self) -> impl Iterator<Item = Requirement<'t>> + use<'t> {
        let bytes = self.bytes;
        let mut at = 8usize;
        (0..self.count).map_while(move |_| {
            let head = bytes.get(at..at + 8)?;
            let what = What::from_u8(*head.first()?)?;
            let flags = *head.get(1)?;
            let index = u16::from_le_bytes([*head.get(2)?, *head.get(3)?]);
            let length = usize::from(*head.get(4)?);
            let name = core::str::from_utf8(bytes.get(at + 8..at + 8 + length)?).ok()?;
            at += 8 + length.next_multiple_of(4);
            Some(Requirement {
                what,
                index,
                name,
                optional: flags & OPTIONAL != 0,
            })
        })
    }
}

/// A requirement the node does not meet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unmet<'t> {
    /// The requirement.
    pub requirement: Requirement<'t>,
    /// Whether the node names it, but the reference is missing, disabled
    /// or cut short, rather than not naming it at all.
    pub unresolved: bool,
}

impl fmt::Display for Unmet<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let requirement = self.requirement;
        let what = requirement.what;
        write!(f, "{}", what.name())?;
        if requirement.index == BY_NAME {
            write!(f, " {}", requirement.name)?;
        } else {
            write!(f, " {}", requirement.index)?;
        }
        let (property, _) = what.properties();
        match what {
            What::PowerSupply => write!(f, " ({}-supply)", requirement.name)?,
            What::PinControl => write!(f, " (pinctrl-*)")?,
            What::ParentBus => write!(f, " (a child device's alone)")?,
            _ => write!(f, " ({property})")?,
        }
        if self.unresolved {
            f.write_str(", which names a node that is missing or disabled")
        } else {
            f.write_str(", which the node does not give")
        }
    }
}

/// What a node offers, beside its identity record: `device_info`'s counts.
#[derive(Clone, Copy, Debug)]
pub struct Offered<'r> {
    /// The node's identity record.
    pub record: Record<'r>,
    /// Apertures minted.
    pub apertures: u32,
    /// Vectors minted.
    pub vectors: u32,
}

/// Hold `offered` to every requirement of `table`: the first one it does
/// not meet, or `Ok`.
///
/// # Errors
///
/// [`Unmet`], naming the requirement.
pub fn check<'t>(table: &Table<'t>, offered: &Offered<'_>) -> Result<(), Unmet<'t>> {
    for requirement in table.iter() {
        match meets(&requirement, offered) {
            Meets::Yes => {}
            Meets::Absent if requirement.optional => {}
            Meets::Absent => {
                return Err(Unmet {
                    requirement,
                    unresolved: false,
                });
            }
            Meets::Unresolved if requirement.optional => {}
            Meets::Unresolved => {
                return Err(Unmet {
                    requirement,
                    unresolved: true,
                });
            }
        }
    }
    Ok(())
}

/// Whether one requirement is met.
enum Meets {
    Yes,
    Absent,
    Unresolved,
}

/// The place `name` has among the strings of the record's `names` property.
fn place_of(record: &Record<'_>, names: &str, name: &str) -> Option<u16> {
    let at = record.names(names).position(|entry| entry == name)?;
    u16::try_from(at).ok()
}

/// Whether the record's `property` has a reference at `index`, resolved.
fn reference(record: &Record<'_>, property: &str, index: u16) -> Meets {
    let found: Option<RecordReference<'_>> = record
        .references()
        .find(|entry| entry.property == property && entry.index == index);
    match found {
        Some(entry) if entry.resolved() => Meets::Yes,
        Some(_) => Meets::Unresolved,
        None if record
            .references()
            .any(|entry| entry.property == property && !entry.resolved()) =>
        {
            // A missing provider ends its property's list: what came after
            // it could not be read.
            Meets::Unresolved
        }
        None => Meets::Absent,
    }
}

fn meets(requirement: &Requirement<'_>, offered: &Offered<'_>) -> Meets {
    let record = &offered.record;
    let what = requirement.what;
    let (property, names) = what.properties();
    let place = if requirement.index == BY_NAME {
        match what {
            What::PowerSupply | What::ParentBus => Some(0),
            What::PinControl if requirement.name == "default" => {
                place_of(record, names, requirement.name).or(Some(0))
            }
            _ => place_of(record, names, requirement.name),
        }
    } else {
        Some(requirement.index)
    };
    let Some(place) = place else {
        return Meets::Absent;
    };
    let within = |count: u32| {
        if u32::from(place) < count {
            Meets::Yes
        } else {
            Meets::Absent
        }
    };
    match what {
        What::Registers => within(offered.apertures),
        What::Interrupt => within(offered.vectors),
        What::Clock | What::Reset => reference(record, property, place),
        What::PowerSupply => {
            let mut supply = [0u8; 264];
            let name = requirement.name.as_bytes();
            let suffix = b"-supply";
            let Some(slot) = supply.get_mut(..name.len() + suffix.len()) else {
                return Meets::Absent;
            };
            let (head, tail) = slot.split_at_mut(name.len());
            head.copy_from_slice(name);
            tail.copy_from_slice(suffix);
            match core::str::from_utf8(slot) {
                Ok(property) => reference(record, property, 0),
                Err(_) => Meets::Absent,
            }
        }
        What::PinControl => {
            let mut state = [0u8; 16];
            let mut writer = Writer {
                bytes: &mut state,
                at: 0,
            };
            if fmt::write(&mut writer, format_args!("pinctrl-{place}")).is_err() {
                return Meets::Absent;
            }
            let length = writer.at;
            let Some(property) = state
                .get(..length)
                .and_then(|b| core::str::from_utf8(b).ok())
            else {
                return Meets::Absent;
            };
            let mut any = false;
            for entry in record
                .references()
                .filter(|entry| entry.property == property)
            {
                any = true;
                if !entry.resolved() {
                    return Meets::Unresolved;
                }
            }
            if any { Meets::Yes } else { Meets::Absent }
        }
        What::ParentBus => Meets::Absent,
    }
}

/// A `fmt::Write` into a fixed buffer.
struct Writer<'b> {
    bytes: &'b mut [u8],
    at: usize,
}

impl fmt::Write for Writer<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.at + text.len();
        let slot = self.bytes.get_mut(self.at..end).ok_or(fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.at = end;
        Ok(())
    }
}

/// Where the section `name` lies in an ELF image read through `read`
/// (`read(offset, buffer)` fills `buffer` from the image at `offset`, or
/// says it could not): `(offset, size)` in the file. 32- and 64-bit
/// little-endian images, the ones Ferrix's drivers are.
pub fn find_section(
    mut read: impl FnMut(u64, &mut [u8]) -> bool,
    name: &str,
) -> Option<(u64, u64)> {
    let mut header = [0u8; 64];
    if !read(0, &mut header) || header.get(..4) != Some(&b"\x7fELF"[..]) {
        return None;
    }
    let wide = match header.get(4) {
        Some(1) => false,
        Some(2) => true,
        _ => return None,
    };
    if header.get(5) != Some(&1) {
        return None;
    }
    let half = |at: usize| Some(u16::from_le_bytes([*header.get(at)?, *header.get(at + 1)?]));
    let (offset, entry, count, names) = if wide {
        (
            u64::from_le_bytes(header.get(0x28..0x30)?.try_into().ok()?),
            half(0x3A)?,
            half(0x3C)?,
            half(0x3E)?,
        )
    } else {
        (
            u64::from(u32::from_le_bytes(header.get(0x20..0x24)?.try_into().ok()?)),
            half(0x2E)?,
            half(0x30)?,
            half(0x32)?,
        )
    };
    if count > 1024 || usize::from(entry) > 64 || entry < if wide { 64 } else { 40 } {
        return None;
    }
    let section = |read: &mut dyn FnMut(u64, &mut [u8]) -> bool, index: u16| {
        let mut bytes = [0u8; 64];
        let at = offset.checked_add(u64::from(index) * u64::from(entry))?;
        if !read(at, bytes.get_mut(..usize::from(entry))?) {
            return None;
        }
        let word = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let dword = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
        // (name, offset, size)
        if wide {
            Some((word(0)?, dword(0x18)?, dword(0x20)?))
        } else {
            Some((word(0)?, u64::from(word(0x10)?), u64::from(word(0x14)?)))
        }
    };
    let (_, strings, strings_size) = section(&mut read, names)?;
    let wanted = name.as_bytes();
    for index in 0..count {
        let Some((name_at, at, size)) = section(&mut read, index) else {
            continue;
        };
        if u64::from(name_at) + wanted.len() as u64 >= strings_size {
            continue;
        }
        let mut found = [0u8; 64];
        let slot = found.get_mut(..=wanted.len())?;
        if read(strings + u64::from(name_at), slot)
            && slot.get(..wanted.len()) == Some(wanted)
            && slot.get(wanted.len()) == Some(&0)
        {
            return Some((at, size));
        }
    }
    None
}
