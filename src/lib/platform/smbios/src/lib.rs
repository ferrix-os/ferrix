//! SMBIOS reader: the firmware's entry point and structure table, checked,
//! walked, and the identity fields Linux shows in `/sys/class/dmi/id`.
//!
//! The loader uses [`EntryPoint::parse`] to decide whether firmware's entry
//! point is one and how long its table is; the kernel uses [`Smbios::new`]
//! on the loader's copy to check it again and decode it once. Both read
//! bytes firmware wrote, so every function here returns a value or an error
//! for any byte string: fields are read with bounds-checked slice access,
//! and the crate is `#![forbid(unsafe_code)]`. `docs/DMI.md` is the design.
//!
//! What it decodes, and how, is Linux's (`drivers/firmware/dmi_scan.c` and
//! `dmi-id.c`, 6.18): the same fields from the same offsets, the same walk
//! with the same stopping rules, the same text. The one deliberate
//! difference: a 2.x entry point whose own checksum fails is refused, where
//! Linux would still take the `_DMI_` header after it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(clippy::arithmetic_side_effects)]

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

/// The most bytes an entry point has: Linux reads 32 and keeps at most 32.
pub const ENTRY_POINT_MAX: usize = 32;

/// The longest structure table the loader copies and the kernel accepts:
/// 1 MiB. A 2.x table cannot be longer than 64 KiB; a 3.x entry point gives
/// a 32-bit maximum, and real machines' tables are tens of KiB at most. A
/// longer one is refused, not cut.
pub const TABLE_MAX: u32 = 1 << 20;

/// The longest text a field's file holds, newline included: one page less
/// the terminator `scnprintf` keeps (`dmi-id.c` `sys_dmi_field_show`).
const FIELD_TEXT_MAX: usize = 4095;

/// Why an entry point or a table was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Neither `_SM3_` nor `_SM_` at the start.
    Anchor,
    /// Fewer bytes than the entry point says it has.
    Short,
    /// The entry point's own length is outside what its version allows.
    Length,
    /// The entry point's bytes do not sum to zero.
    Checksum,
    /// A 2.x entry point's `_DMI_` header is missing or does not sum to zero.
    DmiHeader,
    /// The table is empty.
    EmptyTable,
    /// The table is longer than [`TABLE_MAX`].
    TableTooLong,
    /// The table given is not the length the entry point says.
    TableLength,
}

/// Which entry point it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The 32-bit one, `_SM_` with `_DMI_` at byte 16.
    V2,
    /// The 64-bit one, `_SM3_`.
    V3,
}

/// A checked entry point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryPoint {
    /// Which one.
    pub kind: Kind,
    /// Its own length in bytes, as it says: what `smbios_entry_point` holds.
    pub len: u8,
    /// The version as Linux's `dmi_ver` holds it: `0xMMmmdd`.
    pub version: u32,
    /// The table's physical address, as firmware wrote it.
    pub table_address: u64,
    /// The table's length (2.x) or maximum length (3.x) in bytes.
    pub table_len: u32,
    /// How many structures the table holds (2.x), or 0: no count, the walk
    /// ends at type 127.
    pub count: u16,
}

/// The bytes at `at..at + N`, if there are that many.
fn bytes<const N: usize>(data: &[u8], at: usize) -> Option<[u8; N]> {
    data.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// Whether `data` sums to zero, modulo 256.
fn sums_to_zero(data: &[u8]) -> bool {
    data.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) == 0
}

impl EntryPoint {
    /// Check the entry point at the start of `data` (up to
    /// [`ENTRY_POINT_MAX`] bytes are looked at): its anchor, its length, its
    /// checksums, and its table's length against [`TABLE_MAX`].
    ///
    /// Verifies the same as `dmi_smbios3_present` and `dmi_present`, but for
    /// the 2.x legacy fallback named in the crate documentation.
    pub fn parse(data: &[u8]) -> Result<EntryPoint, Error> {
        let data = data.get(..ENTRY_POINT_MAX).unwrap_or(data);
        let point = if data.starts_with(b"_SM3_") {
            Self::v3(data)?
        } else if data.starts_with(b"_SM_") {
            Self::v2(data)?
        } else {
            return Err(Error::Anchor);
        };
        if point.table_len == 0 {
            return Err(Error::EmptyTable);
        }
        if point.table_len > TABLE_MAX {
            return Err(Error::TableTooLong);
        }
        Ok(point)
    }

    /// The 64-bit entry point: length at 6 (24 to 32), version at 7-9, table
    /// maximum length at 12, address at 16.
    fn v3(data: &[u8]) -> Result<EntryPoint, Error> {
        let len = *data.get(6).ok_or(Error::Short)?;
        if !(24..=32).contains(&len) {
            return Err(Error::Length);
        }
        let own = data.get(..usize::from(len)).ok_or(Error::Short)?;
        if !sums_to_zero(own) {
            return Err(Error::Checksum);
        }
        let [major, minor, docrev] = bytes::<3>(own, 7).ok_or(Error::Short)?;
        Ok(EntryPoint {
            kind: Kind::V3,
            len,
            version: u32::from(major) << 16 | u32::from(minor) << 8 | u32::from(docrev),
            table_len: u32::from_le_bytes(bytes(own, 12).ok_or(Error::Short)?),
            table_address: u64::from_le_bytes(bytes(own, 16).ok_or(Error::Short)?),
            count: 0,
        })
    }

    /// The 32-bit entry point: length at 5 (30 to 32: 31 is the
    /// specification's, 30 the mistake of its version 2.1), version at 6-7,
    /// and the `_DMI_` header's 15 bytes at 16, with its own checksum, the
    /// table length at 22, address at 24 and count at 28.
    fn v2(data: &[u8]) -> Result<EntryPoint, Error> {
        let len = *data.get(5).ok_or(Error::Short)?;
        if !(30..=32).contains(&len) {
            return Err(Error::Length);
        }
        if !sums_to_zero(data.get(..usize::from(len)).ok_or(Error::Short)?) {
            return Err(Error::Checksum);
        }
        let dmi = data.get(16..31).ok_or(Error::Short)?;
        if !dmi.starts_with(b"_DMI_") || !sums_to_zero(dmi) {
            return Err(Error::DmiHeader);
        }
        let version = corrected(u16::from_be_bytes(bytes(data, 6).ok_or(Error::Short)?));
        Ok(EntryPoint {
            kind: Kind::V2,
            len,
            version: u32::from(version) << 8,
            table_len: u32::from(u16::from_le_bytes(bytes(data, 22).ok_or(Error::Short)?)),
            table_address: u64::from(u32::from_le_bytes(bytes(data, 24).ok_or(Error::Short)?)),
            count: u16::from_le_bytes(bytes(data, 28).ok_or(Error::Short)?),
        })
    }
}

/// A 2.x version as Linux corrects some firmware's (`dmi_present`): 2.31
/// and 2.33 are read as 2.3, 2.51 as 2.6. It decides the UUID's byte order
/// and nothing else.
const fn corrected(version: u16) -> u16 {
    match version {
        0x021F | 0x0221 => 0x0203,
        0x0233 => 0x0206,
        other => other,
    }
}

/// An identity field: one file of `/sys/class/dmi/id`, in the order Linux
/// adds them (`dmi_id_init_attr_table`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Type 0, offset 4.
    BiosVendor,
    /// Type 0, offset 5.
    BiosVersion,
    /// Type 0, offset 8.
    BiosDate,
    /// Type 0, offsets 20 and 21.
    BiosRelease,
    /// Type 0, offsets 22 and 23.
    EcFirmwareRelease,
    /// Type 1, offset 4.
    SysVendor,
    /// Type 1, offset 5.
    ProductName,
    /// Type 1, offset 6.
    ProductVersion,
    /// Type 1, offset 7: root only.
    ProductSerial,
    /// Type 1, offsets 8 to 23: root only.
    ProductUuid,
    /// Type 1, offset 26.
    ProductFamily,
    /// Type 1, offset 25.
    ProductSku,
    /// Type 2, offset 4.
    BoardVendor,
    /// Type 2, offset 5.
    BoardName,
    /// Type 2, offset 6.
    BoardVersion,
    /// Type 2, offset 7: root only.
    BoardSerial,
    /// Type 2, offset 8.
    BoardAssetTag,
    /// Type 3, offset 4.
    ChassisVendor,
    /// Type 3, offset 5.
    ChassisType,
    /// Type 3, offset 6.
    ChassisVersion,
    /// Type 3, offset 7: root only.
    ChassisSerial,
    /// Type 3, offset 8.
    ChassisAssetTag,
}

/// How many fields there are.
pub const FIELDS: usize = 22;

impl Field {
    /// Every field, in Linux's order.
    pub const ALL: [Field; FIELDS] = [
        Field::BiosVendor,
        Field::BiosVersion,
        Field::BiosDate,
        Field::BiosRelease,
        Field::EcFirmwareRelease,
        Field::SysVendor,
        Field::ProductName,
        Field::ProductVersion,
        Field::ProductSerial,
        Field::ProductUuid,
        Field::ProductFamily,
        Field::ProductSku,
        Field::BoardVendor,
        Field::BoardName,
        Field::BoardVersion,
        Field::BoardSerial,
        Field::BoardAssetTag,
        Field::ChassisVendor,
        Field::ChassisType,
        Field::ChassisVersion,
        Field::ChassisSerial,
        Field::ChassisAssetTag,
    ];

    /// Its file's name.
    #[must_use]
    pub fn name(self) -> &'static [u8] {
        const NAMES: [&[u8]; FIELDS] = [
            b"bios_vendor",
            b"bios_version",
            b"bios_date",
            b"bios_release",
            b"ec_firmware_release",
            b"sys_vendor",
            b"product_name",
            b"product_version",
            b"product_serial",
            b"product_uuid",
            b"product_family",
            b"product_sku",
            b"board_vendor",
            b"board_name",
            b"board_version",
            b"board_serial",
            b"board_asset_tag",
            b"chassis_vendor",
            b"chassis_type",
            b"chassis_version",
            b"chassis_serial",
            b"chassis_asset_tag",
        ];
        NAMES.get(self.slot()).copied().unwrap_or(b"")
    }

    /// Its file's permission bits, as `dmi-id.c` gives them: the serials and
    /// the UUID name one machine, so only root reads them.
    #[must_use]
    pub const fn mode(self) -> u32 {
        match self {
            Field::ProductSerial
            | Field::ProductUuid
            | Field::BoardSerial
            | Field::ChassisSerial => 0o400,
            _ => 0o444,
        }
    }

    /// Its place in [`Field::ALL`].
    const fn slot(self) -> usize {
        self as usize
    }
}

/// A decoded field's value, borrowed from the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    /// A string's bytes as the table holds them, or empty for index 0 or a
    /// string of spaces only.
    Text(&'a [u8]),
    /// A release, `major.minor`.
    Release(u8, u8),
    /// A UUID's 16 bytes; `true` when its first three fields are
    /// little-endian (SMBIOS 2.6 and later).
    Uuid([u8; 16], bool),
    /// A chassis type, its top bit already dropped.
    Number(u8),
}

/// Every identity field the table had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity<'a> {
    values: [Option<Value<'a>>; FIELDS],
}

impl<'a> Identity<'a> {
    /// `field`'s value, if the table had it.
    #[must_use]
    pub fn get(&self, field: Field) -> Option<Value<'a>> {
        self.values.get(field.slot()).copied().flatten()
    }

    /// Set `field` unless an earlier structure did (`dmi_save_ident`'s
    /// first-wins).
    fn keep(&mut self, field: Field, value: Option<Value<'a>>) {
        if let Some(slot) = self.values.get_mut(field.slot())
            && slot.is_none()
        {
            *slot = value;
        }
    }
}

/// A checked entry point and its walked table.
#[derive(Clone, Copy, Debug)]
pub struct Smbios<'a> {
    /// The entry point's own bytes.
    entry: &'a [u8],
    /// The table, cut after the last structure the walk reached.
    table: &'a [u8],
    /// The entry point, parsed.
    point: EntryPoint,
    /// What the walk decoded.
    identity: Identity<'a>,
    /// How many structures the walk passed.
    structures: u32,
}

impl<'a> Smbios<'a> {
    /// Check `entry` (an entry point, at least its own length) and `table`
    /// (exactly the length the entry point gives), and walk the table.
    pub fn new(entry: &'a [u8], table: &'a [u8]) -> Result<Smbios<'a>, Error> {
        let point = EntryPoint::parse(entry)?;
        let entry = entry.get(..usize::from(point.len)).ok_or(Error::Short)?;
        if u32::try_from(table.len()).ok() != Some(point.table_len) {
            return Err(Error::TableLength);
        }
        let mut identity = Identity {
            values: [None; FIELDS],
        };
        let (used, structures) = walk(table, point.count, |structure| {
            decode(&mut identity, structure, point.version);
        });
        Ok(Smbios {
            entry,
            table: table.get(..used).unwrap_or(table),
            point,
            identity,
            structures,
        })
    }

    /// The entry point's bytes, as firmware wrote them.
    #[must_use]
    pub fn entry_point(&self) -> &'a [u8] {
        self.entry
    }

    /// The table, cut after the last structure the walk reached: what
    /// Linux's `DMI` file holds.
    #[must_use]
    pub fn table(&self) -> &'a [u8] {
        self.table
    }

    /// The entry point, parsed.
    #[must_use]
    pub fn point(&self) -> EntryPoint {
        self.point
    }

    /// How many structures the walk passed.
    #[must_use]
    pub fn structures(&self) -> u32 {
        self.structures
    }

    /// The identity fields.
    #[must_use]
    pub fn identity(&self) -> Identity<'a> {
        self.identity
    }

    /// `field`'s file as Linux shows it: the value, a newline, at most a
    /// page; `None` when the table did not have it.
    pub fn field_text(&self, field: Field, out: &mut impl FnMut(&[u8])) -> Option<()> {
        let value = self.identity.get(field)?;
        let mut sink = Limited {
            left: FIELD_TEXT_MAX - 1,
            out: &mut *out,
        };
        value_text(value, &mut |piece| sink.put(piece));
        out(b"\n");
        Some(())
    }

    /// The `modalias` file: `dmi` and a `:<prefix><value>` per field Linux
    /// lists, values with spaces, control bytes, bytes past 126 and `:`
    /// dropped, a closing `:`, a newline (`dmi-id.c` `get_modalias`).
    pub fn modalias(&self, out: &mut impl FnMut(&[u8])) {
        const PREFIXES: [(&[u8], Field); 15] = [
            (b"bvn", Field::BiosVendor),
            (b"bvr", Field::BiosVersion),
            (b"bd", Field::BiosDate),
            (b"br", Field::BiosRelease),
            (b"efr", Field::EcFirmwareRelease),
            (b"svn", Field::SysVendor),
            (b"pn", Field::ProductName),
            (b"pvr", Field::ProductVersion),
            (b"rvn", Field::BoardVendor),
            (b"rn", Field::BoardName),
            (b"rvr", Field::BoardVersion),
            (b"cvn", Field::ChassisVendor),
            (b"ct", Field::ChassisType),
            (b"cvr", Field::ChassisVersion),
            (b"sku", Field::ProductSku),
        ];
        out(b"dmi");
        // Linux's room: a page less one, less the three of `dmi` and one;
        // each piece is cut to one less than what is left.
        let mut sink = Limited {
            left: FIELD_TEXT_MAX - 5,
            out: &mut *out,
        };
        for (prefix, field) in PREFIXES {
            let Some(value) = self.identity.get(field) else {
                continue;
            };
            sink.put(b":");
            sink.put(prefix);
            value_text(value, &mut |piece| {
                piece
                    .iter()
                    .filter(|&&byte| byte > b' ' && byte < 127 && byte != b':')
                    .for_each(|&byte| sink.put(&[byte]));
            });
        }
        out(b":\n");
    }
}

/// A sink that passes on at most `left` more bytes.
struct Limited<'o, F: FnMut(&[u8])> {
    left: usize,
    out: &'o mut F,
}

impl<F: FnMut(&[u8])> Limited<'_, F> {
    fn put(&mut self, data: &[u8]) {
        let take = data.len().min(self.left);
        if take > 0 {
            (self.out)(data.get(..take).unwrap_or(&[]));
            self.left = self.left.saturating_sub(take);
        }
    }
}

/// `number` in decimal.
fn decimal(number: u8, out: &mut dyn FnMut(&[u8])) {
    let digit = |value: u8| b'0'.saturating_add(value % 10);
    let digits = [digit(number / 100), digit(number / 10), digit(number)];
    let skip = if number >= 100 {
        0
    } else if number >= 10 {
        1
    } else {
        2
    };
    out(digits.get(skip..).unwrap_or(&[]));
}

/// A value's text, as Linux formats it, without the newline.
fn value_text(value: Value<'_>, out: &mut dyn FnMut(&[u8])) {
    match value {
        Value::Text(text) => out(text),
        Value::Release(major, minor) => {
            decimal(major, out);
            out(b".");
            decimal(minor, out);
        }
        Value::Number(number) => decimal(number, out),
        Value::Uuid(bytes, little_endian) => {
            // `lib/uuid.c`: guid_index for `%pUl`, uuid_index for `%pUb`.
            const GUID: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
            const UUID: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let order = if little_endian { GUID } else { UUID };
            for (at, index) in order.into_iter().enumerate() {
                let byte = bytes.get(index).copied().unwrap_or(0);
                out(&[
                    HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'0'),
                    HEX.get(usize::from(byte & 15)).copied().unwrap_or(b'0'),
                ]);
                if matches!(at, 3 | 5 | 7 | 9) {
                    out(b"-");
                }
            }
        }
    }
}

/// Walk `table` as `dmi_decode_table` does, handing `decode` each structure
/// whose strings end inside the table (its formatted area, its strings and
/// the two NULs that end them), and say how many bytes the walk used and how
/// many structures it passed.
///
/// It stops after `count` structures when `count` is not 0, at type 127 when
/// it is, at a structure shorter than its 4-byte header, and where a header
/// would not fit. No byte outside `table` is read.
pub fn walk<'a>(table: &'a [u8], count: u16, mut decode: impl FnMut(&'a [u8])) -> (usize, u32) {
    let len = table.len();
    let mut data = 0usize;
    let mut passed = 0u32;
    // `data` never passes `len` by more than a structure's length and two,
    // so the saturating additions below never saturate; they are how the
    // arithmetic is shown not to wrap.
    let last = len.saturating_sub(1);
    let byte = |at: usize| table.get(at).copied().unwrap_or(0);
    while (count == 0 || passed < u32::from(count)) && data.saturating_add(4) <= len {
        let start = data;
        let kind = byte(start);
        let length = usize::from(byte(start.saturating_add(1)));
        if length < 4 {
            break;
        }
        data = data.saturating_add(length);
        while data < last && (byte(data) != 0 || byte(data.saturating_add(1)) != 0) {
            data = data.saturating_add(1);
        }
        if data < last
            && let Some(structure) = table.get(start..data.saturating_add(2))
        {
            decode(structure);
        }
        data = data.saturating_add(2);
        passed = passed.saturating_add(1);
        if count == 0 && kind == 127 {
            break;
        }
    }
    (data.min(len), passed)
}

/// String `index` of `structure` (`dmi_string_nosave`): empty for index 0,
/// for an index past the strings, and for a string of spaces only.
fn string(structure: &[u8], index: u8) -> &[u8] {
    if index == 0 {
        return &[];
    }
    let length = usize::from(structure.get(1).copied().unwrap_or(0));
    let mut rest = structure.get(length..).unwrap_or(&[]);
    let mut left = index;
    loop {
        let end = rest
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(rest.len());
        left = left.saturating_sub(1);
        if left == 0 || end == 0 {
            let found = rest.get(..end).unwrap_or(&[]);
            return if found.iter().all(|&byte| byte == b' ') {
                &[]
            } else {
                found
            };
        }
        rest = rest.get(end.saturating_add(1)..).unwrap_or(&[]);
    }
}

/// The string field at `offset` (`dmi_save_ident`): absent when the
/// formatted area does not reach it.
fn text_at(structure: &[u8], offset: usize) -> Option<Value<'_>> {
    let length = usize::from(*structure.get(1)?);
    if length <= offset {
        return None;
    }
    Some(Value::Text(string(structure, *structure.get(offset)?)))
}

/// The release whose minor byte is at `index` (`dmi_save_release`): absent
/// when the formatted area is shorter than `index`, or both bytes are 0xFF.
fn release_at(structure: &[u8], index: usize) -> Option<Value<'_>> {
    let length = usize::from(*structure.get(1)?);
    if length < index {
        return None;
    }
    let [major, minor] = bytes::<2>(structure, index.checked_sub(1)?)?;
    if major == 0xFF && minor == 0xFF {
        return None;
    }
    Some(Value::Release(major, minor))
}

/// The UUID at `index` (`dmi_save_uuid`): absent when it does not fit, or is
/// all 0x00 or all 0xFF.
fn uuid_at(structure: &[u8], index: usize, version: u32) -> Option<Value<'_>> {
    let length = usize::from(*structure.get(1)?);
    if length < index.saturating_add(16) {
        return None;
    }
    let uuid = bytes::<16>(structure, index)?;
    if uuid.iter().all(|&byte| byte == 0) || uuid.iter().all(|&byte| byte == 0xFF) {
        return None;
    }
    Some(Value::Uuid(uuid, version >= 0x02_06_00))
}

/// The chassis type at `index` (`dmi_save_type`).
fn type_at(structure: &[u8], index: usize) -> Option<Value<'_>> {
    let length = usize::from(*structure.get(1)?);
    if length <= index {
        return None;
    }
    Some(Value::Number(*structure.get(index)? & 0x7F))
}

/// One structure's identity fields (`dmi_decode`, types 0 to 3).
fn decode<'a>(identity: &mut Identity<'a>, structure: &'a [u8], version: u32) {
    let text = |offset| text_at(structure, offset);
    match structure.first() {
        Some(0) => {
            identity.keep(Field::BiosVendor, text(4));
            identity.keep(Field::BiosVersion, text(5));
            identity.keep(Field::BiosDate, text(8));
            identity.keep(Field::BiosRelease, release_at(structure, 21));
            identity.keep(Field::EcFirmwareRelease, release_at(structure, 23));
        }
        Some(1) => {
            identity.keep(Field::SysVendor, text(4));
            identity.keep(Field::ProductName, text(5));
            identity.keep(Field::ProductVersion, text(6));
            identity.keep(Field::ProductSerial, text(7));
            identity.keep(Field::ProductUuid, uuid_at(structure, 8, version));
            identity.keep(Field::ProductSku, text(25));
            identity.keep(Field::ProductFamily, text(26));
        }
        Some(2) => {
            identity.keep(Field::BoardVendor, text(4));
            identity.keep(Field::BoardName, text(5));
            identity.keep(Field::BoardVersion, text(6));
            identity.keep(Field::BoardSerial, text(7));
            identity.keep(Field::BoardAssetTag, text(8));
        }
        Some(3) => {
            identity.keep(Field::ChassisVendor, text(4));
            identity.keep(Field::ChassisType, type_at(structure, 5));
            identity.keep(Field::ChassisVersion, text(6));
            identity.keep(Field::ChassisSerial, text(7));
            identity.keep(Field::ChassisAssetTag, text(8));
        }
        _ => {}
    }
}
