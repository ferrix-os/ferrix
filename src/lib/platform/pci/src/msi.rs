//! MSI: the older message-signalled interrupt, programmed in configuration
//! space rather than in a table in a BAR.
//!
//! A function with MSI and no MSI-X -- NVIDIA's consumer GPUs among them
//! (`docs/NVIDIA.md` §2.3), and QEMU's `edu` and AHCI -- raises its
//! interrupts by writing one data word to one address, both of which the
//! kernel puts in the capability. The capability's layout depends on two
//! bits of its message control word: whether the address is 64 bits wide,
//! and whether the function can mask its vectors one by one. This module is
//! that layout, so the kernel never computes an offset itself.
//!
//! Only one message is ever enabled: the Multiple Message Enable field is
//! left at zero. More than one would need a block of consecutive vectors,
//! aligned to its size, from an allocator that hands them out one by one,
//! and nothing driven today asks for a second.

use crate::capability::Capability;
use crate::{Address, ConfigSpace};

/// Offset of the message control word, from the capability.
pub const CONTROL: u16 = 0x2;
/// Message control: MSI is enabled.
pub const CONTROL_ENABLE: u16 = 1 << 0;
/// Message control: Multiple Message Capable, bits 3:1.
pub const CONTROL_MULTIPLE_CAPABLE: u16 = 0x7 << 1;
/// Message control: Multiple Message Enable, bits 6:4.
pub const CONTROL_MULTIPLE_ENABLE: u16 = 0x7 << 4;
/// Message control: the address register is 64 bits wide.
pub const CONTROL_64_BIT: u16 = 1 << 7;
/// Message control: the function masks each vector by its bit in the mask
/// register.
pub const CONTROL_PER_VECTOR_MASK: u16 = 1 << 8;
/// Offset of the message address's low half, from the capability.
pub const ADDRESS_LOW: u16 = 0x4;
/// Offset of the message address's high half, with a 64-bit address.
pub const ADDRESS_HIGH: u16 = 0x8;

/// A function's MSI capability, decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Msi {
    /// Where the capability starts.
    pub capability: u16,
    /// Whether the address may be above 4 GiB.
    pub wide: bool,
    /// Whether each vector can be masked on its own.
    pub maskable: bool,
    /// How many messages the function could use, a power of two from 1 to
    /// 32. Only the first is ever enabled.
    pub capable: u8,
}

impl Msi {
    /// Decode the MSI capability `capability` of `function`.
    pub fn read<C: ConfigSpace + ?Sized>(
        space: &C,
        function: Address,
        capability: Capability,
    ) -> Self {
        let control = space.read16(function, capability.offset + CONTROL);
        let log2 = ((control & CONTROL_MULTIPLE_CAPABLE) >> 1).min(5);
        Msi {
            capability: capability.offset,
            wide: control & CONTROL_64_BIT != 0,
            maskable: control & CONTROL_PER_VECTOR_MASK != 0,
            capable: 1 << log2,
        }
    }

    /// Offset of the message data word, from the start of configuration
    /// space.
    #[must_use]
    pub const fn data(&self) -> u16 {
        self.capability + if self.wide { 0xC } else { 0x8 }
    }

    /// Offset of the mask register, if each vector can be masked.
    #[must_use]
    pub const fn mask(&self) -> Option<u16> {
        if !self.maskable {
            return None;
        }
        Some(self.capability + if self.wide { 0x10 } else { 0xC })
    }

    /// Whether a message at `address` can be programmed at all: a 32-bit
    /// capability cannot reach above 4 GiB.
    #[must_use]
    pub const fn reaches(&self, address: u64) -> bool {
        self.wide || address >> 32 == 0
    }

    /// The message control word that enables one message, from `control`:
    /// MSI on and Multiple Message Enable at one message, every other bit
    /// as it was.
    #[must_use]
    pub const fn enabled(control: u16) -> u16 {
        (control & !CONTROL_MULTIPLE_ENABLE) | CONTROL_ENABLE
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeMap;

    use super::{CONTROL_ENABLE, CONTROL_MULTIPLE_ENABLE, Msi};
    use crate::capability::{Capability, ID_MSI};
    use crate::{Address, ConfigSpace};

    /// One function's 16-bit registers, enough for a capability.
    struct Space(BTreeMap<u16, u16>);

    impl ConfigSpace for Space {
        fn read8(&self, _: Address, offset: u16) -> u8 {
            self.read16(Address::new(0, 0, 0, 0).unwrap(), offset & !1) as u8
        }
        fn read16(&self, _: Address, offset: u16) -> u16 {
            self.0.get(&offset).copied().unwrap_or(0)
        }
        fn read32(&self, function: Address, offset: u16) -> u32 {
            u32::from(self.read16(function, offset))
                | u32::from(self.read16(function, offset + 2)) << 16
        }
        fn write16(&mut self, _: Address, offset: u16, value: u16) {
            let _ = self.0.insert(offset, value);
        }
        fn write32(&mut self, function: Address, offset: u16, value: u32) {
            self.write16(function, offset, value as u16);
            self.write16(function, offset + 2, (value >> 16) as u16);
        }
    }

    fn decode(control: u16) -> Msi {
        let function = Address::new(0, 0, 5, 0).unwrap();
        let space = Space([(0x6A, control)].into_iter().collect());
        Msi::read(
            &space,
            function,
            Capability {
                function,
                id: ID_MSI,
                offset: 0x68,
            },
        )
    }

    #[test]
    fn a_64_bit_capability_without_masking_is_nvidia_s_layout() {
        // GA106's: 64-bit, one message, not maskable (`docs/NVIDIA.md`).
        let msi = decode(0x0080);
        assert!(msi.wide && !msi.maskable);
        assert_eq!((msi.capable, msi.data(), msi.mask()), (1, 0x74, None));
        assert!(msi.reaches(0xFEE0_0000) && msi.reaches(1 << 40));
    }

    #[test]
    fn the_data_and_mask_registers_move_with_the_address_width() {
        let narrow = decode(0x0100);
        assert_eq!((narrow.data(), narrow.mask()), (0x70, Some(0x74)));
        assert!(narrow.reaches(0xFEE0_0000) && !narrow.reaches(1 << 32));
        let wide = decode(0x0180);
        assert_eq!((wide.data(), wide.mask()), (0x74, Some(0x78)));
    }

    #[test]
    fn multiple_message_capable_is_a_power_of_two_never_past_32() {
        assert_eq!(decode(0x3 << 1).capable, 8);
        assert_eq!(decode(0x5 << 1).capable, 32);
        // 6 and 7 are reserved; read as the most there can be.
        assert_eq!(decode(0x7 << 1).capable, 32);
    }

    #[test]
    fn enabling_asks_for_one_message_and_keeps_the_other_bits() {
        let control = 0x0180 | 0x3 << 1 | 0x2 << 4;
        let enabled = Msi::enabled(control);
        assert_eq!(enabled & CONTROL_MULTIPLE_ENABLE, 0);
        assert_eq!(enabled & CONTROL_ENABLE, CONTROL_ENABLE);
        assert_eq!(
            enabled & !(CONTROL_MULTIPLE_ENABLE | CONTROL_ENABLE),
            control & !CONTROL_MULTIPLE_ENABLE
        );
    }
}
