//! A PCI function's `modalias` and the lines its bus adds to `uevent`.
//!
//! `lspci` reads the `uevent` for `PCI_CLASS`, `PCI_ID`, `PCI_SUBSYS_ID` and
//! `PCI_SLOT_NAME`, and a module loader matches `MODALIAS` against driver
//! tables, so both are `pci_uevent`'s formats exactly: upper-case hexadecimal,
//! the class with at least four digits and no prefix.

use alloc::vec::Vec;

use crate::text::put;
use crate::uevent;

/// What configuration space says a function is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The vendor identifier.
    pub vendor: u16,
    /// The device identifier.
    pub device: u16,
    /// The board's vendor, from a type 0 header; zero for a bridge.
    pub subsystem_vendor: u16,
    /// The board's own identifier.
    pub subsystem_device: u16,
    /// The class code: base class in bits 23:16, subclass in 15:8, the
    /// programming interface in 7:0.
    pub class: u32,
}

/// The modalias, without its newline: `pci:v00001AF4d00001050sv…`.
fn put_modalias(out: &mut Vec<u8>, ids: &Identity) {
    let [interface, sub, base, _] = ids.class.to_le_bytes();
    put(
        out,
        format_args!(
            "pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{base:02X}sc{sub:02X}i{interface:02X}",
            ids.vendor, ids.device, ids.subsystem_vendor, ids.subsystem_device
        ),
    );
}

/// The `modalias` file.
pub fn modalias(out: &mut Vec<u8>, ids: &Identity) {
    put_modalias(out, ids);
    out.push(b'\n');
}

/// The `uevent` file: `DRIVER` when one is bound, then `pci_uevent`'s five.
pub fn uevent(out: &mut Vec<u8>, ids: &Identity, slot: &[u8], driver: Option<&[u8]>) {
    if let Some(driver) = driver {
        uevent::driver(out, driver);
    }
    put(
        out,
        format_args!("PCI_CLASS={:04X}\n", ids.class & 0x00ff_ffff),
    );
    put(
        out,
        format_args!("PCI_ID={:04X}:{:04X}\n", ids.vendor, ids.device),
    );
    put(
        out,
        format_args!(
            "PCI_SUBSYS_ID={:04X}:{:04X}\n",
            ids.subsystem_vendor, ids.subsystem_device
        ),
    );
    uevent::var(out, "PCI_SLOT_NAME", slot);
    out.extend_from_slice(b"MODALIAS=");
    put_modalias(out, ids);
    out.push(b'\n');
}
