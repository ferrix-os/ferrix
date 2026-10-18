//! Reaching the firmware tables from the kernel.
//!
//! `ferrix_acpi` never dereferences a pointer: it asks a [`Tables`] for bytes
//! at a physical address. This is the kernel's answer to that question, and it
//! is the only place in the tree that turns a number firmware wrote into a
//! reference the parser will read.
//!
//! Two things it refuses to do, both of which are the difference between a
//! parser that is hostile-input-safe and a kernel that is not:
//!
//! * an address outside the direct map is `None` rather than a wild read, and
//! * a length that would run past the end of the direct map is `None` too,
//!   even though the address itself is fine.
//!
//! Firmware is not an attacker, but it is software somebody else wrote and
//! shipped years ago, and the RSDP is the one pointer the kernel is handed
//! without having computed it.

use ferrix_acpi::{Acpi, AcpiError, RootKind, Rsdp, Tables};
use ferrix_bootinfo::BootView;

/// Physical memory, as the table parser is allowed to see it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DirectMap {
    /// Virtual address physical address zero appears at.
    base: u64,
    /// Bytes of physical address space the direct map covers.
    len: u64,
}

impl Tables for DirectMap {
    fn table(&self, physical_address: u64, len: usize) -> Option<&[u8]> {
        let end = physical_address.checked_add(len as u64)?;
        if end > self.len {
            return None;
        }
        let at = self.base.checked_add(physical_address)?;

        // SAFETY: the range [physical_address, end) is inside the direct map,
        // checked immediately above, and the direct map is a live read-only
        // alias of physical memory for the whole life of the system. The
        // lifetime is tied to `&self`, and `DirectMap` outlives every table
        // reference taken from it.
        Some(unsafe { core::slice::from_raw_parts(at as *const u8, len) })
    }
}

/// The firmware tables, rooted and ready to walk.
#[derive(Debug)]
pub(crate) struct Firmware {
    /// How the parser reads physical memory.
    memory: DirectMap,
    /// Physical address of the XSDT or RSDT.
    root: u64,
    /// Which of the two it is.
    kind: RootKind,
}

impl Firmware {
    /// Find and validate the root table the loader's RSDP points at.
    ///
    /// # Errors
    ///
    /// Any of `ferrix_acpi`'s parse errors, plus [`AcpiError::Unreadable`] for
    /// an RSDP the loader did not report at all — which is not a failure on
    /// AArch64, where the device tree is the other way to describe a machine.
    pub(crate) fn open(view: &BootView<'_>) -> Result<Self, AcpiError> {
        let info = view.raw();
        let memory = DirectMap {
            base: info.physmap_base,
            len: info.physmap_len,
        };

        if info.rsdp == 0 {
            return Err(AcpiError::Unreadable(0));
        }

        let rsdp = Rsdp::read(&memory, info.rsdp)?;
        let (root, kind) = rsdp.root_table()?;
        Ok(Firmware { memory, root, kind })
    }

    /// The table set, borrowed from this structure's own view of memory.
    pub(crate) fn acpi(&self) -> Acpi<'_, DirectMap> {
        Acpi::new(&self.memory, self.root, self.kind)
    }
}
