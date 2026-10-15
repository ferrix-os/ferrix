//! Reaching the device tree from the kernel.
//!
//! The counterpart of `crate::discovery::acpi` for machines that describe themselves with
//! a flattened device tree: every ARMv7-A board, and AArch64 ones whose
//! firmware chooses to. `ferrix_fdt` parses a byte slice and never follows a
//! pointer; this is where that slice comes from — the loader's copy of
//! firmware's tree, read through the direct map.
//!
//! The copy is in memory the map reports as `MemKind::DeviceTree`, which
//! nothing reclaims and nothing writes, so a tree borrowed from it is
//! `'static`, honestly: stage 10 enumerates devices from the same bytes long
//! after boot.

use ferrix_bootinfo::BootView;
use ferrix_fdt::Fdt;

/// The device tree the loader handed over.
///
/// # Errors
///
/// If there is none, if it is not inside the direct map, or if it does not
/// parse. The first is the ordinary case on x86-64 and on AArch64 under ACPI,
/// and an error only to a caller that needed one.
pub fn open(view: &BootView<'_>) -> Result<Fdt<'static>, &'static str> {
    const OUTSIDE: &str = "the device tree is not inside the direct map";

    let (phys, len) = view
        .device_tree()
        .ok_or("the loader handed over no device tree")?;
    let info = view.raw();

    // Checked against both ends of the direct map rather than trusted: the
    // address is one the kernel was handed, not one it computed.
    let offset = phys.checked_sub(info.physmap_phys).ok_or(OUTSIDE)?;
    if offset
        .checked_add(len)
        .is_none_or(|end| end > info.physmap_len)
    {
        return Err(OUTSIDE);
    }
    let at = info.physmap_base + offset;

    // SAFETY: (BOOT-DATA) the range is inside the direct map, checked above, which is a
    // live mapping of RAM for the life of the system. The loader copied the
    // tree there into memory nothing reclaims or writes, so a shared borrow of
    // it for `'static` aliases no writer.
    let blob = unsafe { core::slice::from_raw_parts(at as *const u8, len as usize) };
    Fdt::parse(blob).map_err(|_| "the device tree does not parse")
}
