//! Firmware's SMBIOS tables, as the loader copied them (`docs/DMI.md` §3.2).
//!
//! The loader checked firmware's entry point, copied it and the structure
//! table into memory the map reports as `MemKind::Smbios`, and said where in
//! `BootInfo`. This module accepts that copy or refuses it, once, at boot:
//! [`range`] decides from numbers alone whether the copy is where the kernel
//! may read it, [`decode`] checks it again with `ferrix_smbios` and walks it,
//! and [`open`] keeps the result for sysfs, which shows it.
//!
//! It fails closed: a copy that is absent or refused leaves nothing kept, and
//! then `/sys/class/dmi` and `/sys/firmware/dmi` do not exist. It never stops
//! the boot. The copy is borrowed, not copied again: the memory is the
//! loader's, which nothing reclaims or writes.

use ferrix_bootinfo::{BootView, MemKind, MemRegion, SMBIOS_ENTRY_BYTES};
use ferrix_smbios::{Error, Smbios, TABLE_MAX, Value};
use ferrix_sync::Once;

/// Why the loader's copy was refused, or that there was none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The loader handed over no copy.
    None,
    /// The copy's length is not an entry point and a table of at most
    /// `TABLE_MAX` bytes.
    Length,
    /// The copy's end overflows.
    Overflow,
    /// The copy is not wholly inside one region the map reports as
    /// `MemKind::Smbios`.
    Region,
    /// The copy is not wholly inside the direct map.
    DirectMap,
    /// `ferrix_smbios` refused it on reading it again.
    Parse(Error),
}

impl Refusal {
    /// Its words, for the boot line.
    pub const fn text(self) -> &'static str {
        match self {
            Refusal::None => "none handed over",
            Refusal::Length => "the copy's length is impossible",
            Refusal::Overflow => "the copy's end overflows",
            Refusal::Region => "the copy is not inside the SMBIOS region of the memory map",
            Refusal::DirectMap => "the copy is not inside the direct map",
            Refusal::Parse(Error::Anchor) => "no anchor",
            Refusal::Parse(Error::Short) => "the entry point is cut short",
            Refusal::Parse(Error::Length) => "the entry point's length is impossible",
            Refusal::Parse(Error::Checksum) => "bad checksum",
            Refusal::Parse(Error::DmiHeader) => "bad _DMI_ header",
            Refusal::Parse(Error::EmptyTable) => "empty table",
            Refusal::Parse(Error::TableTooLong) => "table longer than 1 MiB",
            Refusal::Parse(Error::TableLength) => {
                "the copy's length is not the entry point's table and 32"
            }
        }
    }
}

/// Where the direct map is: its physical origin and how many bytes it spans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Direct {
    /// The physical address at its base.
    pub(crate) phys: u64,
    /// Its length in bytes.
    pub(crate) len: u64,
}

/// Whether the copy the loader offered, `(address, length)`, may be read:
/// its offset into the direct map and its length, or why not.
///
/// A pure function of its arguments, so the boot check feeds it forged
/// ones (`discovery::smbios_check`). `region_of` answers which region of the
/// memory map holds an address.
pub(crate) fn range(
    offered: Option<(u64, u64)>,
    region_of: impl Fn(u64) -> Option<MemRegion>,
    direct: Direct,
) -> Result<(u64, u64), Refusal> {
    let (at, len) = offered.ok_or(Refusal::None)?;
    let most = SMBIOS_ENTRY_BYTES.saturating_add(u64::from(TABLE_MAX));
    if len <= SMBIOS_ENTRY_BYTES || len > most {
        return Err(Refusal::Length);
    }
    let end = at.checked_add(len).ok_or(Refusal::Overflow)?;
    let region = region_of(at).ok_or(Refusal::Region)?;
    if region.kind != MemKind::Smbios || at < region.base || end > region.end() {
        return Err(Refusal::Region);
    }
    let offset = at.checked_sub(direct.phys).ok_or(Refusal::DirectMap)?;
    if offset.checked_add(len).is_none_or(|stop| stop > direct.len) {
        return Err(Refusal::DirectMap);
    }
    Ok((offset, len))
}

/// Read `copy` again: the entry point in its first `SMBIOS_ENTRY_BYTES`, the
/// table after them, exactly as long as the entry point says.
pub(crate) fn decode(copy: &[u8]) -> Result<Smbios<'_>, Refusal> {
    let split = usize::try_from(SMBIOS_ENTRY_BYTES).map_err(|_| Refusal::Length)?;
    let (entry, table) = copy.split_at_checked(split).ok_or(Refusal::Length)?;
    Smbios::new(entry, table).map_err(Refusal::Parse)
}

/// What [`open`] decided, kept for sysfs.
static KEPT: Once<Result<Smbios<'static>, Refusal>> = Once::new();

/// Accept or refuse the loader's copy, once; later calls answer the same.
pub fn open(view: &BootView<'_>) -> Result<Smbios<'static>, Refusal> {
    *KEPT.call_once(|| {
        let info = view.raw();
        let direct = Direct {
            phys: info.physmap_phys,
            len: info.physmap_len,
        };
        let (offset, len) = range(view.smbios(), |at| view.region_of(at).copied(), direct)?;
        let at = info
            .physmap_base
            .checked_add(offset)
            .ok_or(Refusal::DirectMap)?;
        let count = usize::try_from(len).map_err(|_| Refusal::Length)?;
        // SAFETY: (BOOT-DATA) `range` put the copy wholly inside one region the map
        // reports as `MemKind::Smbios` and wholly inside the direct map, which
        // is a live mapping of RAM for the life of the system. The loader copied
        // the tables there, and nothing reclaims or writes that kind of memory,
        // so a shared borrow of it for `'static` aliases no writer.
        let copy = unsafe { core::slice::from_raw_parts(at as *const u8, count) };
        decode(copy)
    })
}

/// The tables, if [`open`] accepted them.
pub fn tables() -> Option<Smbios<'static>> {
    KEPT.get().copied().and_then(Result::ok)
}

/// A string field's bytes as the boot line shows them, everything outside
/// printable ASCII and the backslash escaped as `\xNN`: firmware's bytes
/// reach the console and, netbooted, a server. `-` for a field the table
/// does not have.
#[derive(Clone, Copy, Debug)]
pub struct Shown<'a>(pub Option<Value<'a>>);

impl core::fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Some(Value::Text(text)) = self.0 else {
            return f.write_str("-");
        };
        for &byte in text {
            if (0x20..0x7F).contains(&byte) && byte != b'\\' {
                f.write_fmt(format_args!("{}", char::from(byte)))?;
            } else {
                f.write_fmt(format_args!("\\x{byte:02x}"))?;
            }
        }
        Ok(())
    }
}
