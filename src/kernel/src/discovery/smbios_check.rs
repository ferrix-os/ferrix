//! The SMBIOS copy's acceptance, fed forged inputs (`docs/DMI.md` §4).
//!
//! [`super::smbios::range`] and [`super::smbios::decode`] decide whether the
//! loader's copy is read at all. The copy a boot is handed exercises one
//! path through them; this feeds them a copy that is absent, outside every
//! region of its kind, straddling its region's end, outside the direct map,
//! overflowing, one byte longer than its table, and with a bad checksum, and
//! requires each refused by name, and a well-formed one accepted.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::{MemKind, MemRegion, PAGE_SIZE, SMBIOS_ENTRY_BYTES};
use ferrix_smbios::Error;

use super::smbios::{Direct, Refusal, decode, range};

/// What the check counted, for the boot line.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Report {
    /// Forged inputs refused by the name they had to be.
    pub(crate) refused: u32,
}

/// The forged machine: one SMBIOS region of two pages at 1 MiB, a usable one
/// after it, and a direct map from 0 over 16 MiB.
const REGION: MemRegion = MemRegion {
    base: 0x10_0000,
    len: 2 * PAGE_SIZE,
    kind: MemKind::Smbios,
    reserved: 0,
};

/// The usable region after it.
const USABLE: MemRegion = MemRegion {
    base: 0x10_0000 + 2 * PAGE_SIZE,
    len: 0x10_0000,
    kind: MemKind::Usable,
    reserved: 0,
};

/// The forged direct map.
const DIRECT: Direct = Direct {
    phys: 0,
    len: 0x100_0000,
};

/// Which forged region holds `at`.
fn region_of(at: u64) -> Option<MemRegion> {
    [REGION, USABLE]
        .into_iter()
        .find(|region| region.contains(at))
}

/// A well-formed copy: a 3.0 entry point padded to 32 bytes, and a table of
/// one end-of-table structure, `extra` bytes longer than the entry point says
/// when `extra` is not 0.
fn copy(extra: usize) -> Vec<u8> {
    let table: &[u8] = &[127, 4, 0, 0, 0, 0];
    let table_len = u32::try_from(table.len()).unwrap_or(0).to_le_bytes();
    let mut entry = vec![0u8; 32];
    for (at, byte) in b"_SM3_".iter().enumerate() {
        if let Some(slot) = entry.get_mut(at) {
            *slot = *byte;
        }
    }
    let fields = [(6, 24u8), (7, 3), (8, 0), (10, 1)];
    for (at, value) in fields {
        if let Some(slot) = entry.get_mut(at) {
            *slot = value;
        }
    }
    for (at, byte) in (12..).zip(table_len) {
        if let Some(slot) = entry.get_mut(at) {
            *slot = byte;
        }
    }
    let sum = entry
        .iter()
        .take(24)
        .fold(0u8, |sum, &byte| sum.wrapping_add(byte));
    if let Some(slot) = entry.get_mut(5) {
        *slot = 0u8.wrapping_sub(sum);
    }
    entry.extend_from_slice(table);
    entry.resize(entry.len().saturating_add(extra), 0);
    entry
}

/// Require `got` to be the refusal `wanted`, counting it.
fn refused<T>(
    report: &mut Report,
    got: &Result<T, Refusal>,
    wanted: Refusal,
    problem: &'static str,
) -> Result<(), &'static str> {
    match got {
        Err(refusal) if *refusal == wanted => {
            report.refused = report.refused.saturating_add(1);
            Ok(())
        }
        _ => Err(problem),
    }
}

/// Run every forged case.
///
/// Verifies: L.discovery.5
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let good = u64::try_from(copy(0).len()).unwrap_or(0);
    let accepted = range(Some((REGION.base, good)), region_of, DIRECT);
    if accepted != Ok((REGION.base, good)) {
        return Err("a copy inside its region and the direct map was refused");
    }

    refused(
        &mut report,
        &range(None, region_of, DIRECT),
        Refusal::None,
        "no copy was not refused as none",
    )?;
    refused(
        &mut report,
        &range(Some((USABLE.base, good)), region_of, DIRECT),
        Refusal::Region,
        "a copy in usable memory was not refused as outside its region",
    )?;
    refused(
        &mut report,
        &range(Some((REGION.end() - 8, good)), region_of, DIRECT),
        Refusal::Region,
        "a copy straddling its region's end was not refused as outside its region",
    )?;
    refused(
        &mut report,
        &range(
            Some((REGION.base, good)),
            region_of,
            Direct {
                phys: REGION.base + PAGE_SIZE,
                len: DIRECT.len,
            },
        ),
        Refusal::DirectMap,
        "a copy below the direct map was not refused as outside it",
    )?;
    refused(
        &mut report,
        &range(Some((u64::MAX - 8, good)), region_of, DIRECT),
        Refusal::Overflow,
        "a copy whose end overflows was not refused as overflowing",
    )?;
    refused(
        &mut report,
        &range(Some((REGION.base, SMBIOS_ENTRY_BYTES)), region_of, DIRECT),
        Refusal::Length,
        "a copy holding no table was not refused for its length",
    )?;

    if decode(&copy(0)).is_err() {
        return Err("a well-formed copy did not decode");
    }
    refused(
        &mut report,
        &decode(&copy(1)),
        Refusal::Parse(Error::TableLength),
        "a copy one byte longer than its table was not refused for its length",
    )?;
    let mut bad = copy(0);
    if let Some(byte) = bad.get_mut(20) {
        *byte ^= 1;
    }
    refused(
        &mut report,
        &decode(&bad),
        Refusal::Parse(Error::Checksum),
        "a copy with a bad checksum was not refused for it",
    )?;
    Ok(report)
}
