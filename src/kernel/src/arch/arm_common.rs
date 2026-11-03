//! Arm peripherals, not Arm architectures: register-level drivers for hardware
//! both Arm ISAs can have, which the architecture that uses them has already
//! found in the machine's description -- the MADT on AArch64, the device tree
//! on ARMv7-A.
//!
//! The GICv2 and the PL011 are shared by both. The STM32MP1's USART is
//! ARMv7-A's, which has to pick between it and a PL011 because the machines it
//! targets do not agree on one: `armv7a::console` is where the device tree
//! decides, as `aarch64::console` decides between a PL011 and what the Pixel 7
//! has, and the layering check keeps generic code from naming any of them. A
//! GICv3 is AArch64's alone, because its CPU interface is system registers,
//! and lives in `aarch64::gic`.

pub(super) mod gicv2;
pub(super) mod pl011;
pub(super) mod smt;
#[cfg(target_arch = "arm")]
pub(super) mod stm32_usart;
