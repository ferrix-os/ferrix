//! HID report descriptors: what a device says its reports hold, parsed into
//! fields, and the fields' values read out of a report.
//!
//! A report descriptor is a stream of short items (HID 1.11 §6.2.2): *main*
//! items make a field of the report or open and close a collection, *global*
//! items set what every later field inherits (usage page, logical range,
//! size, count, report ID) and *local* items the usages of the next main item
//! only. This module keeps what an input or output field needs and nothing
//! else: physical ranges, units, designators and strings are read past.
//!
//! No allocation: the fields and their usages go in fixed pools, sized well
//! past what a keyboard or mouse describes. A descriptor that overflows them
//! keeps what fitted and says it was cut.

/// The most fields kept, inputs and outputs together.
pub const MAX_FIELDS: usize = 32;
/// The most usages kept, over every field.
pub const MAX_USAGES: usize = 96;
/// The most values one field reads from a report: enough for a keyboard
/// that reports every key as a bit.
pub const MAX_COUNT: usize = 255;
/// The deepest the global item stack goes (`Push`).
const MAX_PUSH: usize = 4;

/// A usage, as a page in the high half and an ID in the low.
pub type Usage = u32;

/// The usage page and ID of `usage`.
#[must_use]
pub const fn split(usage: Usage) -> (u16, u16) {
    ((usage >> 16) as u16, usage as u16)
}

/// Generic Desktop's page.
pub const PAGE_DESKTOP: u16 = 0x01;
/// The keyboard page.
pub const PAGE_KEYBOARD: u16 = 0x07;
/// The LED page.
pub const PAGE_LED: u16 = 0x08;
/// The button page.
pub const PAGE_BUTTON: u16 = 0x09;
/// The consumer page.
pub const PAGE_CONSUMER: u16 = 0x0C;

/// A field's direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Direction {
    /// In an input report.
    #[default]
    Input,
    /// In an output report.
    Output,
}

/// Where a field's usages are.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Usages {
    /// None: padding, or a field this module does not map.
    #[default]
    None,
    /// `Usage Minimum` to `Usage Maximum`.
    Range(Usage, Usage),
    /// Listed `Usage` items: `len` of the usage pool from `first`.
    List {
        /// Where they start in [`Descriptor`]'s pool.
        first: u16,
        /// How many.
        len: u16,
    },
}

/// One input or output field: `count` values of `size` bits each.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Field {
    /// Input or output.
    pub direction: Direction,
    /// The report it is in; 0 in a descriptor without report IDs.
    pub report: u8,
    /// Its first bit, counted from after the report ID.
    pub offset: u16,
    /// Bits per value.
    pub size: u8,
    /// Values.
    pub count: u8,
    /// A constant field is padding.
    pub constant: bool,
    /// A variable field has a value per usage; an array field's values are
    /// the usages that are on.
    pub variable: bool,
    /// A relative value is a change, not a state.
    pub relative: bool,
    /// `Logical Minimum`.
    pub minimum: i32,
    /// `Logical Maximum`.
    pub maximum: i32,
    /// The usages.
    pub usages: Usages,
    /// The application collection it is in.
    pub application: Usage,
}

/// A parsed report descriptor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Descriptor {
    fields: [Field; MAX_FIELDS],
    count: usize,
    pool: [Usage; MAX_USAGES],
    used: usize,
    /// Whether reports start with a report ID byte.
    pub numbered: bool,
    /// Whether something did not fit and was left out.
    pub cut: bool,
}

impl Default for Descriptor {
    fn default() -> Self {
        Descriptor {
            fields: [Field::default(); MAX_FIELDS],
            count: 0,
            pool: [0; MAX_USAGES],
            used: 0,
            numbered: false,
            cut: false,
        }
    }
}

/// What the global items have set.
#[derive(Clone, Copy, Debug, Default)]
struct Globals {
    page: u16,
    minimum: i32,
    maximum: i32,
    size: u32,
    count: u32,
    report: u8,
}

/// What the local items of the next main item have set.
#[derive(Clone, Copy, Debug, Default)]
struct Locals {
    minimum: Option<Usage>,
    maximum: Option<Usage>,
    first: usize,
    len: usize,
}

/// One short item.
#[derive(Clone, Copy, Debug)]
struct Item {
    tag: u8,
    unsigned: u32,
    length: usize,
}

impl Item {
    fn signed(&self) -> i32 {
        sign_extend(self.unsigned, self.length)
    }
}

/// A descriptor being parsed, and the item state it is parsed with.
struct Parser {
    parsed: Descriptor,
    globals: Globals,
    stack: [Globals; MAX_PUSH],
    depth: usize,
    locals: Locals,
    /// The application collection's usage, and how deep inside it.
    application: Usage,
    nesting: u32,
    /// Bits used so far per report, inputs and outputs apart.
    offsets: [[u32; 256]; 2],
}

impl Parser {
    /// A main item: a field, or a collection opened or closed. The locals
    /// are the next main item's no longer.
    fn main(&mut self, item: &Item) -> Result<(), Error> {
        match item.tag {
            0x8 | 0x9 => self.field(item)?,
            0xA => {
                // An application collection names the fields inside it.
                if self.nesting == 0 && item.unsigned == 1 {
                    self.application = self.parsed.first_usage(&self.locals, self.globals.page);
                }
                self.nesting += 1;
            }
            0xC => self.nesting = self.nesting.saturating_sub(1),
            _ => {}
        }
        self.locals = Locals {
            first: self.parsed.used,
            ..Locals::default()
        };
        Ok(())
    }

    fn field(&mut self, item: &Item) -> Result<(), Error> {
        let direction = if item.tag == 0x8 {
            Direction::Input
        } else {
            Direction::Output
        };
        let globals = self.globals;
        let offset = self
            .offsets
            .get_mut(usize::from(direction == Direction::Output))
            .and_then(|table| table.get_mut(usize::from(globals.report)))
            .ok_or(Error::TooLarge)?;
        let start = *offset;
        *offset = offset.saturating_add(globals.size.saturating_mul(globals.count));
        if *offset > 8 * 8192 {
            return Err(Error::TooLarge);
        }
        self.parsed.add_field(
            &globals,
            &self.locals,
            item.unsigned,
            direction,
            start,
            self.application,
        )
    }

    fn global(&mut self, item: &Item) {
        let globals = &mut self.globals;
        match item.tag {
            0x0 => globals.page = item.unsigned as u16,
            0x1 => globals.minimum = item.signed(),
            0x2 => globals.maximum = item.signed(),
            0x7 => globals.size = item.unsigned,
            0x8 => {
                globals.report = item.unsigned as u8;
                self.parsed.numbered = true;
            }
            0x9 => globals.count = item.unsigned,
            0xA => {
                if let Some(slot) = self.stack.get_mut(self.depth) {
                    *slot = *globals;
                    self.depth += 1;
                }
            }
            0xB => {
                let saved = self.depth.checked_sub(1).and_then(|depth| {
                    self.depth = depth;
                    self.stack.get(depth).copied()
                });
                if let Some(saved) = saved {
                    self.globals = saved;
                }
            }
            _ => {}
        }
    }

    fn local(&mut self, item: &Item) {
        let usage = extend(item.unsigned, item.length, self.globals.page);
        match item.tag {
            0x0 => match self.parsed.pool.get_mut(self.parsed.used) {
                Some(slot) => {
                    *slot = usage;
                    self.parsed.used += 1;
                    self.locals.len += 1;
                }
                None => self.parsed.cut = true,
            },
            0x1 => self.locals.minimum = Some(usage),
            0x2 => self.locals.maximum = Some(usage),
            _ => {}
        }
    }
}

/// Why a descriptor could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// An item ran past the end.
    Truncated,
    /// A field is too wide to read, or a report too long to address.
    TooLarge,
}

impl Descriptor {
    /// Parse `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] for an item cut short, [`Error::TooLarge`] for a
    /// field of more than 32 bits a value or a report past 8 KiB.
    pub fn parse(bytes: &[u8]) -> Result<Descriptor, Error> {
        let mut parser = Parser {
            parsed: Descriptor::default(),
            globals: Globals::default(),
            stack: [Globals::default(); MAX_PUSH],
            depth: 0,
            locals: Locals::default(),
            application: 0,
            nesting: 0,
            offsets: [[0; 256]; 2],
        };
        let mut at = 0_usize;
        while let Some(&prefix) = bytes.get(at) {
            if prefix == 0xFE {
                // A long item: its length is the next byte.
                let length = usize::from(*bytes.get(at + 1).ok_or(Error::Truncated)?);
                at += 3 + length;
                continue;
            }
            let length = match prefix & 0x3 {
                3 => 4,
                size => usize::from(size),
            };
            let data = bytes.get(at + 1..at + 1 + length).ok_or(Error::Truncated)?;
            at += 1 + length;
            let item = Item {
                tag: prefix >> 4,
                unsigned: data
                    .iter()
                    .rev()
                    .fold(0_u32, |value, &byte| (value << 8) | u32::from(byte)),
                length,
            };
            match (prefix >> 2) & 0x3 {
                0 => parser.main(&item)?,
                1 => parser.global(&item),
                2 => parser.local(&item),
                _ => {}
            }
        }
        Ok(parser.parsed)
    }

    fn first_usage(&self, locals: &Locals, page: u16) -> Usage {
        if locals.len > 0 {
            self.pool.get(locals.first).copied().unwrap_or(0)
        } else {
            locals.minimum.unwrap_or(u32::from(page) << 16)
        }
    }

    fn add_field(
        &mut self,
        globals: &Globals,
        locals: &Locals,
        flags: u32,
        direction: Direction,
        offset: u32,
        application: Usage,
    ) -> Result<(), Error> {
        if globals.size > 32 {
            return Err(Error::TooLarge);
        }
        let usages = match (locals.len, locals.minimum, locals.maximum) {
            (0, Some(minimum), Some(maximum)) if minimum <= maximum => {
                Usages::Range(minimum, maximum)
            }
            (0, _, _) => Usages::None,
            (len, _, _) => Usages::List {
                first: u16::try_from(locals.first).unwrap_or(u16::MAX),
                len: u16::try_from(len).unwrap_or(0),
            },
        };
        let field = Field {
            direction,
            report: globals.report,
            offset: u16::try_from(offset).map_err(|_| Error::TooLarge)?,
            size: globals.size as u8,
            count: u8::try_from(globals.count.min(MAX_COUNT as u32)).unwrap_or(0),
            constant: flags & 1 != 0,
            variable: flags & 2 != 0,
            relative: flags & 4 != 0,
            minimum: globals.minimum,
            // A maximum given in fewer bytes than its sign needs reads as
            // negative; HID means it unsigned then, as Linux reads it.
            maximum: if globals.maximum < globals.minimum {
                i32::try_from(globals.maximum.cast_unsigned() & mask(globals.size))
                    .unwrap_or(i32::MAX)
            } else {
                globals.maximum
            },
            usages,
            application,
        };
        match self.fields.get_mut(self.count) {
            Some(slot) => {
                *slot = field;
                self.count += 1;
            }
            None => self.cut = true,
        }
        Ok(())
    }

    /// The fields, in the order the descriptor gives them.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        self.fields.get(..self.count).unwrap_or(&[])
    }

    /// The usage of value `index` of `field`: for a variable field the
    /// usage it reports, the last one repeating past the list's end as HID
    /// says; for an array field the usage its value `index` names.
    #[must_use]
    pub fn usage(&self, field: &Field, index: u32) -> Option<Usage> {
        match field.usages {
            Usages::None => None,
            Usages::Range(minimum, maximum) => {
                let usage = minimum.checked_add(index)?;
                (usage <= maximum).then_some(usage)
            }
            Usages::List { first, len } => {
                if len == 0 {
                    return None;
                }
                let at = u32::from(first) + index.min(u32::from(len) - 1);
                self.pool.get(usize::try_from(at).ok()?).copied()
            }
        }
    }

    /// Every usage `field` can report.
    pub fn all_usages<'a>(&'a self, field: &'a Field) -> impl Iterator<Item = Usage> + 'a {
        let count = match field.usages {
            Usages::None => 0,
            Usages::Range(minimum, maximum) => {
                maximum.saturating_sub(minimum).saturating_add(1).min(0x400)
            }
            Usages::List { len, .. } => u32::from(len),
        };
        (0..count).filter_map(move |index| self.usage(field, index))
    }

    /// How many bytes output report `report` takes, not counting its ID.
    #[must_use]
    pub fn output_bytes(&self, report: u8) -> usize {
        let bits = self
            .fields()
            .iter()
            .filter(|field| field.direction == Direction::Output && field.report == report)
            .map(|field| {
                usize::from(field.offset) + usize::from(field.size) * usize::from(field.count)
            })
            .max()
            .unwrap_or(0);
        bits.div_ceil(8)
    }
}

/// A value of `length` bytes as a signed number.
fn sign_extend(value: u32, length: usize) -> i32 {
    match length {
        1 => i32::from(value as u8 as i8),
        2 => i32::from(value as u16 as i16),
        _ => value.cast_signed(),
    }
}

/// A usage item's value: a four-byte one carries its own page, a shorter one
/// takes the current page.
fn extend(value: u32, length: usize, page: u16) -> Usage {
    if length == 4 {
        value
    } else {
        (u32::from(page) << 16) | (value & 0xFFFF)
    }
}

/// The low `bits` bits.
const fn mask(bits: u32) -> u32 {
    if bits >= 32 {
        u32::MAX
    } else {
        (1 << bits) - 1
    }
}

/// Value `index` of `field` in `report` (the report without its ID byte):
/// sign-extended when the field's minimum is negative. `None` past the
/// report's end.
#[must_use]
pub fn value(field: &Field, report: &[u8], index: u32) -> Option<i32> {
    let size = u32::from(field.size);
    if size == 0 {
        return None;
    }
    let first = u32::from(field.offset).checked_add(index.checked_mul(size)?)?;
    let mut raw = 0_u64;
    // A value wider than 32 bits (a vendor's blob, which nothing maps) keeps
    // its low 32: a shift past the accumulator's width would be a panic.
    for bit in 0..size.min(32) {
        let at = first + bit;
        let byte = *report.get(usize::try_from(at / 8).ok()?)?;
        if byte & (1 << (at % 8)) != 0 {
            raw |= 1 << bit;
        }
    }
    let raw = raw as u32;
    if field.minimum < 0 && size < 32 && raw & (1 << (size - 1)) != 0 {
        Some((raw | !mask(size)).cast_signed())
    } else {
        Some(raw.cast_signed())
    }
}

/// Set value `index` of `field` in `report` to `value`'s low bits.
pub fn set_value(field: &Field, report: &mut [u8], index: u32, value: u32) {
    let size = u32::from(field.size);
    let Some(first) = index
        .checked_mul(size)
        .and_then(|bits| u32::from(field.offset).checked_add(bits))
    else {
        return;
    };
    for bit in 0..size {
        let at = first + bit;
        if let Some(byte) = usize::try_from(at / 8).ok().and_then(|i| report.get_mut(i)) {
            if value & (1 << bit) != 0 {
                *byte |= 1 << (at % 8);
            } else {
                *byte &= !(1 << (at % 8));
            }
        }
    }
}
