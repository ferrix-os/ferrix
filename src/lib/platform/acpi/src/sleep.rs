//! What it takes to switch a PC off: the soft-off state S5 (ACPI 6.5 §7.4.2,
//! §16.1.7), as far as the kernel's last port writes need it.
//!
//! The FADT names the PM1 control registers; the DSDT's `\_S5` package names
//! the `SLP_TYP` values that select S5 in them. The DSDT is AML, and this is
//! not an AML interpreter: [`s5_sleep_types`] finds a `Name (_S5, Package
//! (n) { a, b, ... })` and reads its first two constants, and answers nothing
//! for any other shape. [`SleepControl::of`] fails closed: any part missing,
//! unusable or ambiguous, and nothing is recorded, so the machine is stopped
//! as it was before S5 was known (the certification consultant's OK IF,
//! ledger line 4704, conditions 3 and 4).

use crate::{Acpi, Fadt, GenericAddress, Tables, read_table, u8_at, u32_at};

/// Signature of the Differentiated System Description Table.
pub const DSDT_SIGNATURE: [u8; 4] = *b"DSDT";

/// FADT flags bit 20: `HW_REDUCED_ACPI`, a machine with no fixed hardware,
/// whose sleep registers are elsewhere. Not driven.
pub const HW_REDUCED_ACPI: u32 = 1 << 20;

/// AML's `NameOp`.
const NAME_OP: u8 = 0x08;
/// AML's `PackageOp`.
const PACKAGE_OP: u8 = 0x12;
/// The root prefix, `\`.
const ROOT_CHAR: u8 = b'\\';
/// The constants a sleep type may be written as.
const ZERO_OP: u8 = 0x00;
const ONE_OP: u8 = 0x01;
const BYTE_PREFIX: u8 = 0x0A;
const WORD_PREFIX: u8 = 0x0B;
const DWORD_PREFIX: u8 = 0x0C;
/// `SLP_TYP` is three bits wide.
const SLEEP_TYPE_MASK: u32 = 0x7;

/// Everything the kernel writes to enter S5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepControl {
    /// `PM1a_CNT`'s I/O port.
    pub pm1a: u16,
    /// `PM1b_CNT`'s I/O port, where the machine has a second block.
    pub pm1b: Option<u16>,
    /// `SLP_TYPa` for S5.
    pub sleep_type_a: u8,
    /// `SLP_TYPb` for S5.
    pub sleep_type_b: u8,
    /// `SMI_CMD`'s I/O port, 0 for none: written with [`Self::acpi_enable`]
    /// to hand the machine to ACPI when firmware has not.
    pub smi_command: u16,
    /// The value that hands it over, 0 for none.
    pub acpi_enable: u8,
}

/// A PM1 control block's port, by the FADT's 64-bit field when firmware
/// filled it in -- which must then be I/O space within 16 bits, with no
/// falling back to the 32-bit field -- else by the 32-bit one. `Ok(None)`
/// for a block that is not there; `Err(())` for one that is and cannot be
/// used.
fn control_port(extended: Option<GenericAddress>, legacy: Option<u32>) -> Result<Option<u16>, ()> {
    if let Some(address) = extended.filter(GenericAddress::is_present) {
        if address.address_space_id != GenericAddress::SYSTEM_IO {
            return Err(());
        }
        return u16::try_from(address.address).map(Some).map_err(|_| ());
    }
    match legacy {
        None | Some(0) => Ok(None),
        Some(port) => u16::try_from(port).map(Some).map_err(|_| ()),
    }
}

impl Fadt<'_> {
    /// `SMI_CMD`, the port that takes [`Self::acpi_enable`].
    #[must_use]
    pub fn smi_command(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 48)
    }

    /// `ACPI_ENABLE`, written to `SMI_CMD` to hand the machine to ACPI.
    #[must_use]
    pub fn acpi_enable(&self) -> Option<u8> {
        u8_at(self.table.bytes(), 52)
    }

    /// `PM1a_CNT_BLK`, the 32-bit field.
    #[must_use]
    pub fn pm1a_control_block(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 64)
    }

    /// `PM1b_CNT_BLK`, the 32-bit field.
    #[must_use]
    pub fn pm1b_control_block(&self) -> Option<u32> {
        u32_at(self.table.bytes(), 68)
    }

    /// `PM1_CNT_LEN`, each control block's width in bytes.
    #[must_use]
    pub fn pm1_control_length(&self) -> Option<u8> {
        u8_at(self.table.bytes(), 89)
    }

    /// `X_PM1a_CNT_BLK`.
    #[must_use]
    pub fn x_pm1a_control_block(&self) -> Option<GenericAddress> {
        GenericAddress::parse(self.table.bytes(), 172)
    }

    /// `X_PM1b_CNT_BLK`.
    #[must_use]
    pub fn x_pm1b_control_block(&self) -> Option<GenericAddress> {
        GenericAddress::parse(self.table.bytes(), 184)
    }
}

impl SleepControl {
    /// S5's registers and values from `fadt` and the DSDT's bytes `dsdt`,
    /// or `None` when anything needed is missing, unusable or ambiguous.
    #[must_use]
    pub fn of(fadt: &Fadt<'_>, dsdt: &[u8]) -> Option<SleepControl> {
        if fadt.flags()? & HW_REDUCED_ACPI != 0 {
            return None;
        }
        if fadt.pm1_control_length()? < 2 {
            return None;
        }
        let pm1a = control_port(fadt.x_pm1a_control_block(), fadt.pm1a_control_block())
            .ok()
            .flatten()?;
        let pm1b = control_port(fadt.x_pm1b_control_block(), fadt.pm1b_control_block()).ok()?;
        let (sleep_type_a, sleep_type_b) = s5_sleep_types(dsdt)?;
        let smi_command = fadt
            .smi_command()
            .and_then(|port| u16::try_from(port).ok())
            .unwrap_or(0);
        Some(SleepControl {
            pm1a,
            pm1b,
            sleep_type_a,
            sleep_type_b,
            smi_command,
            acpi_enable: fadt.acpi_enable().unwrap_or(0),
        })
    }
}

impl<T: Tables> Acpi<'_, T> {
    /// S5's registers and values, or `None` as [`SleepControl::of`] says,
    /// and when the DSDT cannot be read or fails its signature, length or
    /// checksum.
    #[must_use]
    pub fn sleep_control(&self) -> Option<SleepControl> {
        let fadt = self.fadt().ok()?;
        let dsdt = self.dsdt(&fadt)?;
        SleepControl::of(&fadt, dsdt.body())
    }

    /// The DSDT `fadt` points at, checked whole: `None` when the FADT names
    /// none, it cannot be read, is not a DSDT, or its checksum is wrong.
    #[must_use]
    pub fn dsdt(&self, fadt: &Fadt<'_>) -> Option<crate::Table<'_>> {
        let address = fadt.dsdt_address().filter(|&address| address != 0)?;
        let table = read_table(self.tables, address).ok()?;
        table.expect_signature(DSDT_SIGNATURE).ok()?;
        table.checksum_valid().then_some(table)
    }
}

/// An AML `PkgLength` at the start of `bytes`: the length it encodes and
/// how many bytes it took (ACPI 6.5 §20.2.4).
fn package_length(bytes: &[u8]) -> Option<(usize, usize)> {
    let lead = *bytes.first()?;
    let follow = usize::from(lead >> 6);
    if follow == 0 {
        return Some((usize::from(lead & 0x3F), 1));
    }
    let mut length = usize::from(lead & 0x0F);
    for at in 0..follow {
        let byte = usize::from(*bytes.get(at.checked_add(1)?)?);
        length |= byte.checked_shl(u32::try_from(4 + 8 * at).ok()?)?;
    }
    Some((length, follow.checked_add(1)?))
}

/// One integer constant at the start of `bytes`: its value and its size.
fn constant(bytes: &[u8]) -> Option<(u32, usize)> {
    match *bytes.first()? {
        ZERO_OP => Some((0, 1)),
        ONE_OP => Some((1, 1)),
        BYTE_PREFIX => Some((u32::from(*bytes.get(1)?), 2)),
        WORD_PREFIX => {
            let low = u32::from(*bytes.get(1)?);
            let high = u32::from(*bytes.get(2)?);
            Some((low | high << 8, 3))
        }
        DWORD_PREFIX => Some((u32_at(bytes, 1)?, 5)),
        _ => None,
    }
}

/// The package that starts at `bytes` (just after `PackageOp`): its first
/// two elements, each masked to `SLP_TYP`'s three bits.
fn first_two(bytes: &[u8]) -> Option<(u8, u8)> {
    let (length, taken) = package_length(bytes)?;
    // The length counts itself and everything after it, to the package's end.
    let package = bytes.get(..length)?;
    let mut rest = package.get(taken..)?;
    let count = *rest.first()?;
    if count < 2 {
        return None;
    }
    rest = rest.get(1..)?;
    let (a, used) = constant(rest)?;
    rest = rest.get(used..)?;
    let (b, _) = constant(rest)?;
    let mask = |value: u32| u8::try_from(value & SLEEP_TYPE_MASK).ok();
    Some((mask(a)?, mask(b)?))
}

/// `SLP_TYPa` and `SLP_TYPb` for S5, from a DSDT's AML: every `Name (_S5,
/// Package ...)` in it, which must agree, or `None`. A `Method (_S5)`, a
/// package that is not of constants, and `_S5_` anywhere else are no
/// answer.
#[must_use]
pub fn s5_sleep_types(aml: &[u8]) -> Option<(u8, u8)> {
    let mut found: Option<(u8, u8)> = None;
    let mut at = 0usize;
    while let Some(window) = aml.get(at..) {
        let Some(offset) = window.windows(4).position(|name| name == b"_S5_") else {
            break;
        };
        let name = at.checked_add(offset)?;
        at = name.checked_add(1)?;
        let before = name.checked_sub(1).and_then(|i| aml.get(i)).copied();
        let named = match before {
            Some(NAME_OP) => true,
            Some(ROOT_CHAR) => name
                .checked_sub(2)
                .and_then(|i| aml.get(i))
                .is_some_and(|&op| op == NAME_OP),
            _ => false,
        };
        if !named || aml.get(name.checked_add(4)?) != Some(&PACKAGE_OP) {
            continue;
        }
        let types = first_two(aml.get(name.checked_add(5)?..)?)?;
        match found {
            Some(earlier) if earlier != types => return None,
            _ => found = Some(types),
        }
    }
    found
}
