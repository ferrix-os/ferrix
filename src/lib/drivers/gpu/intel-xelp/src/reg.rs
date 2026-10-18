// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/i915_reg_defs.h:
// Copyright © 2022 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! `i915_reg_defs.h`'s helpers, as `const fn`s: a register is a byte offset
//! (`_MMIO`), fields are masks (`REG_GENMASK`), and the high half of a
//! *masked* register says which bits of the low half a write changes
//! (`_MASKED_FIELD`).

/// `i915_reg_t`: a register's byte offset in the register window.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Reg(pub u32);

impl Reg {
    /// `i915_mmio_reg_offset`.
    #[must_use]
    pub const fn offset(self) -> u32 {
        self.0
    }
}

/// `REG_BIT(n)`.
#[must_use]
pub const fn bit(n: u32) -> u32 {
    1 << n
}

/// `REG_GENMASK(high, low)`: bits `low..=high`.
#[must_use]
pub const fn genmask(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) & (u32::MAX << low)
}

/// `REG_FIELD_PREP(mask, value)`: `value` shifted into `mask`.
#[must_use]
pub const fn field_prep(mask: u32, value: u32) -> u32 {
    (value << mask.trailing_zeros()) & mask
}

/// `REG_FIELD_GET(mask, value)`.
#[must_use]
pub const fn field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}

/// `_MASKED_FIELD(mask, value)`: write `value` into the bits of `mask`, of
/// a register whose high 16 bits enable writes to the low 16.
#[must_use]
pub const fn masked_field(mask: u32, value: u32) -> u32 {
    (mask << 16) | value
}

/// `_MASKED_BIT_ENABLE(a)`.
#[must_use]
pub const fn masked_bit_enable(a: u32) -> u32 {
    masked_field(a, a)
}

/// `_MASKED_BIT_DISABLE(a)`.
#[must_use]
pub const fn masked_bit_disable(a: u32) -> u32 {
    masked_field(a, 0)
}
