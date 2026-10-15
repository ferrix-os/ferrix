//! Firmware's SMBIOS tables, copied for the kernel (`docs/DMI.md` §3.1).
//!
//! Firmware publishes the entry point in its configuration table; the entry
//! point says where the structure table is. Under boot services both are
//! identity mapped, so the loader reads them where firmware put them, checks
//! the entry point with `ferrix_smbios`, and copies it and the table into one
//! allocation the map reports as `MemKind::Smbios`, which the kernel keeps.
//! The kernel then reads RAM it was handed, not firmware's memory, which the
//! direct map does not cover.
//!
//! None of this can fail the boot: a table that is absent or refused is
//! reported, and the kernel is handed none.

use core::ptr;

use ferrix_bootinfo::{PAGE_SIZE, SMBIOS_ENTRY_BYTES};
use ferrix_smbios::{ENTRY_POINT_MAX, EntryPoint, Error, Kind};

use crate::console::println;
use crate::services::{Allocation, Services};
use crate::uefi::tables::{MemoryType, SMBIOS_GUID, SMBIOS3_GUID};

/// The copy, and how many of its bytes are real, or `None`.
///
/// The 3.x entry point first, and the 2.x one if the 3.x is absent or
/// refused, as Linux's `dmi_scan_machine` does.
pub(crate) fn copy(services: &Services) -> Option<(Allocation, u64)> {
    for (guid, name) in [(&SMBIOS3_GUID, "3.x"), (&SMBIOS_GUID, "2.x")] {
        let Some(address) = services.configuration_table(guid) else {
            continue;
        };
        match copy_at(services, address) {
            Ok(copied) => return Some(copied),
            Err(why) => println!("  smbios   {name} entry point at {address:#x} refused: {why}"),
        }
    }
    None
}

/// Why `ferrix_smbios` refused an entry point, in a word or two.
const fn reason(error: Error) -> &'static str {
    match error {
        Error::Anchor => "no anchor",
        Error::Short => "cut short",
        Error::Length => "impossible length",
        Error::Checksum => "bad checksum",
        Error::DmiHeader => "bad _DMI_ header",
        Error::EmptyTable => "empty table",
        Error::TableTooLong => "table longer than 1 MiB",
        Error::TableLength => "table length",
    }
}

/// Whether any of `ranges` overlaps what firmware's memory map calls device
/// memory (`EfiMemoryMappedIO`, `EfiMemoryMappedIOPortSpace`), which is no
/// place for a table and not to be read as one.
fn device_memory(services: &Services, ranges: &[(u64, u64)]) -> Result<bool, &'static str> {
    const NO_MAP: &str = "no memory map to check it against";
    let size = services.memory_map_size().map_err(|_| NO_MAP)?;
    let buffer = services
        .allocate(
            "reading the memory map for SMBIOS",
            size,
            MemoryType::LOADER_DATA,
        )
        .map_err(|_| NO_MAP)?;
    let found = services.memory_map(buffer).map(|map| {
        map.entries().any(|descriptor| {
            let start = descriptor.physical_start;
            let end = descriptor
                .number_of_pages
                .checked_mul(PAGE_SIZE)
                .and_then(|len| start.checked_add(len))
                .unwrap_or(u64::MAX);
            matches!(descriptor.memory_type, 11 | 12)
                && ranges
                    .iter()
                    .any(|&(base, len)| base < end && base.saturating_add(len) > start)
        })
    });
    let _ = services.free("freeing the SMBIOS memory map buffer", buffer);
    found.map_err(|_| NO_MAP)
}

/// Check the entry point at `address` and copy it and its table.
fn copy_at(services: &Services, address: u64) -> Result<(Allocation, u64), &'static str> {
    let at = usize::try_from(address)
        .ok()
        .filter(|&at| at != 0)
        .ok_or("not addressable")?;
    // SAFETY: firmware published an SMBIOS entry point at this address, and
    // under boot services all of memory is identity mapped. The longest entry
    // point is 32 bytes, and Linux reads 32 at it too (`dmi_scan.c`).
    let entry = unsafe { core::slice::from_raw_parts(at as *const u8, ENTRY_POINT_MAX) };
    let point = EntryPoint::parse(entry).map_err(reason)?;
    let own = usize::from(point.len);
    let table_len = u64::from(point.table_len);
    let table = usize::try_from(point.table_address)
        .ok()
        .filter(|&table| table != 0 && table.checked_add(point.table_len as usize).is_some())
        .ok_or("table not addressable")?;
    if device_memory(
        services,
        &[
            (address, ENTRY_POINT_MAX as u64),
            (point.table_address, table_len),
        ],
    )? {
        return Err("in device memory");
    }

    let len = SMBIOS_ENTRY_BYTES + table_len;
    let copy = services
        .allocate("copying the SMBIOS tables", len, MemoryType::FERRIX_SMBIOS)
        .map_err(|_| "no memory for the copy")?;
    // SAFETY: `own` is at most 32, inside the entry point read above; the copy
    // is a fresh allocation of at least `len` bytes, zeroed, which the entry
    // point cannot overlap.
    unsafe { ptr::copy_nonoverlapping(at as *const u8, copy.address as *mut u8, own) };
    // SAFETY: firmware's table is `table_len` bytes at `table` by the entry
    // point it checksummed, identity mapped under boot services; the copy has
    // room for it after the first SMBIOS_ENTRY_BYTES, and is fresh.
    unsafe {
        ptr::copy_nonoverlapping(
            table as *const u8,
            (copy.address + SMBIOS_ENTRY_BYTES) as *mut u8,
            point.table_len as usize,
        );
    };
    let version = point.version;
    println!(
        "  smbios   {}.{} entry point ({}), table of {table_len} bytes copied",
        version >> 16,
        (version >> 8) & 0xFF,
        match point.kind {
            Kind::V3 => "64-bit",
            Kind::V2 => "32-bit",
        }
    );
    Ok((copy, len))
}
