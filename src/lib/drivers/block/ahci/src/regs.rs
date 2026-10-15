//! The HBA's registers: offsets, and the bits this crate reads and writes
//! (AHCI 1.3.1, sections 3.1 and 3.3).

/// Host Capabilities.
pub const CAP: u32 = 0x00;
/// Global HBA Control.
pub const GHC: u32 = 0x04;
/// Interrupt Status, one bit a port.
pub const IS: u32 = 0x08;
/// Ports Implemented.
pub const PI: u32 = 0x0C;
/// Version.
pub const VS: u32 = 0x10;
/// Host Capabilities Extended.
pub const CAP2: u32 = 0x24;
/// BIOS/OS Handoff Control and Status.
pub const BOHC: u32 = 0x28;

/// `CAP.S64A`: the HBA reaches 64-bit addresses.
pub const CAP_S64A: u32 = 1 << 31;
/// `CAP.SSS`: staggered spin-up; the host spins each port up with `SUD`.
pub const CAP_SSS: u32 = 1 << 27;
/// `CAP.SCLO`: the HBA can override a busy task file (`PxCMD.CLO`).
pub const CAP_SCLO: u32 = 1 << 24;

/// `GHC.HR`: HBA reset.
pub const GHC_HR: u32 = 1 << 0;
/// `GHC.IE`: interrupts enabled.
pub const GHC_IE: u32 = 1 << 1;
/// `GHC.AE`: AHCI enable.
pub const GHC_AE: u32 = 1 << 31;

/// `CAP2.BOH`: the HBA supports the BIOS/OS handoff.
pub const CAP2_BOH: u32 = 1 << 0;
/// `BOHC.BOS`: the BIOS owns the HBA.
pub const BOHC_BOS: u32 = 1 << 0;
/// `BOHC.OOS`: the OS asks for the HBA.
pub const BOHC_OOS: u32 = 1 << 1;
/// `BOHC.BB`: the BIOS is busy cleaning up.
pub const BOHC_BB: u32 = 1 << 4;

/// Where port 0's registers start.
pub const PORTS: u32 = 0x100;
/// The bytes between two ports' registers.
pub const PORT_STRIDE: u32 = 0x80;
/// The most ports an HBA has.
pub const MAX_PORTS: u8 = 32;

/// Port `port`'s register at `offset` (one of the `PX_*` constants).
#[must_use]
pub const fn port(port: u8, offset: u32) -> u32 {
    PORTS + port as u32 * PORT_STRIDE + offset
}

/// `PxCLB`: command list base, low half (1 KiB aligned).
pub const PX_CLB: u32 = 0x00;
/// `PxCLBU`: command list base, high half.
pub const PX_CLBU: u32 = 0x04;
/// `PxFB`: received FIS base, low half (256 B aligned).
pub const PX_FB: u32 = 0x08;
/// `PxFBU`: received FIS base, high half.
pub const PX_FBU: u32 = 0x0C;
/// `PxIS`: interrupt status, write one to clear.
pub const PX_IS: u32 = 0x10;
/// `PxIE`: interrupt enable.
pub const PX_IE: u32 = 0x14;
/// `PxCMD`: command and status.
pub const PX_CMD: u32 = 0x18;
/// `PxTFD`: task file data, the device's status and error.
pub const PX_TFD: u32 = 0x20;
/// `PxSIG`: the signature of the device attached.
pub const PX_SIG: u32 = 0x24;
/// `PxSSTS`: SATA status.
pub const PX_SSTS: u32 = 0x28;
/// `PxSCTL`: SATA control.
pub const PX_SCTL: u32 = 0x2C;
/// `PxSERR`: SATA error, write one to clear.
pub const PX_SERR: u32 = 0x30;
/// `PxSACT`: native command queuing's active slots.
pub const PX_SACT: u32 = 0x34;
/// `PxCI`: commands issued, one bit a slot.
pub const PX_CI: u32 = 0x38;

/// `PxCMD.ST`: the port processes the command list.
pub const CMD_ST: u32 = 1 << 0;
/// `PxCMD.SUD`: spin up the device.
pub const CMD_SUD: u32 = 1 << 1;
/// `PxCMD.POD`: power on the device.
pub const CMD_POD: u32 = 1 << 2;
/// `PxCMD.CLO`: command list override, clears a stuck `BSY`/`DRQ`.
pub const CMD_CLO: u32 = 1 << 3;
/// `PxCMD.FRE`: the port receives FISes into `PxFB`.
pub const CMD_FRE: u32 = 1 << 4;
/// `PxCMD.FR`: FIS receive is running.
pub const CMD_FR: u32 = 1 << 14;
/// `PxCMD.CR`: the command list is running.
pub const CMD_CR: u32 = 1 << 15;

/// `PxIS.TFES`: the device reported an error in its status.
pub const IS_TFES: u32 = 1 << 30;
/// `PxIS.HBFS`: host bus fatal error.
pub const IS_HBFS: u32 = 1 << 29;
/// `PxIS.HBDS`: host bus data error.
pub const IS_HBDS: u32 = 1 << 28;
/// `PxIS.IFS`: interface fatal error.
pub const IS_IFS: u32 = 1 << 27;
/// Every `PxIS` bit that ends a command in failure.
pub const IS_ERRORS: u32 = IS_TFES | IS_HBFS | IS_HBDS | IS_IFS;

/// `PxTFD` status `ERR`.
pub const TFD_ERR: u32 = 1 << 0;
/// `PxTFD` status `DRQ`.
pub const TFD_DRQ: u32 = 1 << 3;
/// `PxTFD` status `BSY`.
pub const TFD_BSY: u32 = 1 << 7;

/// `PxSSTS.DET`, bits 3:0.
pub const SSTS_DET_MASK: u32 = 0xF;
/// `PxSSTS.DET` = 1: a device is there but the link is not up yet.
pub const SSTS_DET_PRESENT: u32 = 1;
/// `PxSCTL.DET` = 1: perform interface initialisation (COMRESET).
pub const SCTL_DET_INIT: u32 = 1;
/// `PxSCTL.DET` and `PxSCTL.IPM`, bits 3:0 and 11:8.
pub const SCTL_DET_IPM_MASK: u32 = 0xF0F;
/// `PxSCTL.IPM` = 3: no transitions to Partial or Slumber.
pub const SCTL_IPM_NONE: u32 = 0x300;
/// `PxSSTS.DET` = 3: a device is there and the link is up.
pub const SSTS_DET_ESTABLISHED: u32 = 3;

/// `PxSIG` of an ATA disk.
pub const SIG_ATA: u32 = 0x0000_0101;

/// What every register of a function that left the bus reads as.
pub const GONE: u32 = u32::MAX;

/// The page this crate's control memory comes in.
pub const PAGE: usize = 4096;
