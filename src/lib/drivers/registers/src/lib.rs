//! Typed registers, without a kernel: the access types, values and value
//! encodings `ferrix-driver-derive`'s `register_map!` and `bitfields!`
//! generate code against. `ferrix-driver` re-exports all of it and adds the
//! mapping (`Registers`); this crate holds what can be tested on the host.
//!
//! A register is [`ReadWrite`], [`ReadOnly`] or [`WriteOnly`], so a write to a
//! register that only reads does not compile; its value is a `bitfields!`
//! type or a plain integer. A field that the hardware stores encoded takes
//! the real value and encodes it: [`OffsetByOne`] for a field that holds
//! "the value less one", so a driver writes `.frame_size(8)` for eight-bit
//! frames, never 7.
//!
//! # A value out of range
//!
//! Each multi-bit field has two setters. The one named as the field is for a
//! fixed configuration, built in a `const` item, where a value the field
//! cannot hold fails the build:
//!
//! ```compile_fail
//! extern crate ferrix_registers as ferrix_driver;
//!
//! ferrix_driver_derive::bitfields! {
//!     pub Configuration1 {
//!         frame_size: bits 0..=4 as OffsetByOne<4, 32>,
//!     }
//! }
//!
//! const SETUP: Configuration1 = Configuration1::new().frame_size(40);
//!
//! fn main() {
//!     let _ = SETUP;
//! }
//! ```
//!
//! while the same with a value the field holds builds:
//!
//! ```
//! extern crate ferrix_registers as ferrix_driver;
//!
//! ferrix_driver_derive::bitfields! {
//!     pub Configuration1 {
//!         frame_size: bits 0..=4 as OffsetByOne<4, 32>,
//!     }
//! }
//!
//! const SETUP: Configuration1 = Configuration1::new().frame_size(8);
//!
//! fn main() {
//!     assert_eq!(SETUP.bits(), 7);
//! }
//! ```
//!
//! A value known only at run time goes through the fallible setter,
//! `try_<field>`, which answers [`OutOfRange`] instead of stopping the
//! driver.
//!
//! Hand-written rather than `tock-registers`: the design page names its own
//! access types and value encodings, full words throughout, and the macros
//! are a page of `ferrix-driver-derive`; a dependency would bring its own
//! names (`ReadWrite<u32, Register::Register>`, `register_bitfields!`) for
//! the same things.

#![no_std]

// The derive's macros are what the doctests and tests exercise.
#[cfg(test)]
use ferrix_driver_derive as _;

use core::marker::PhantomData;

/// A width a register can be read and written in.
pub trait Width: Copy {
    /// Read one at `at`.
    ///
    /// # Safety
    ///
    /// (DEVICE) `at` is mapped device memory, aligned for `Self`.
    unsafe fn read(at: usize) -> Self;

    /// Write one at `at`.
    ///
    /// # Safety
    ///
    /// (DEVICE) As [`Width::read`], and the mapping is writable.
    unsafe fn write(at: usize, value: Self);
}

/// Read a register as one plain load, never inlined so the address arrives
/// in a register and the load has no writeback a device model cannot
/// emulate (`crate::mmio`'s note).
///
/// # Safety
///
/// (DEVICE) As [`Width::read`].
#[inline(never)]
unsafe fn load<T: Copy>(at: usize) -> T {
    // SAFETY: (DEVICE) the caller's contract: mapped device memory, aligned.
    unsafe { core::ptr::read_volatile(at as *const T) }
}

/// Write a register as one plain store, never inlined, as [`load`].
///
/// # Safety
///
/// (DEVICE) As [`Width::write`].
#[inline(never)]
unsafe fn store<T: Copy>(at: usize, value: T) {
    // SAFETY: (DEVICE) the caller's contract: mapped, writable, aligned.
    unsafe { core::ptr::write_volatile(at as *mut T, value) }
}

macro_rules! widths {
    ($($width:ty),*) => {$(
        impl Width for $width {
            unsafe fn read(at: usize) -> Self {
                // SAFETY: (DEVICE) the trait's contract, passed on.
                unsafe { load(at) }
            }

            unsafe fn write(at: usize, value: Self) {
                // SAFETY: (DEVICE) the trait's contract, passed on.
                unsafe { store(at, value) }
            }
        }

        impl Value for $width {
            type Bits = $width;

            fn into_bits(self) -> $width {
                self
            }

            fn from_bits(bits: $width) -> Self {
                bits
            }
        }
    )*};
}

/// A register's value: a `bitfields!` type, or a plain integer.
pub trait Value: Copy {
    /// What the register holds.
    type Bits: Width;

    /// The bits to write.
    fn into_bits(self) -> Self::Bits;

    /// The value read.
    fn from_bits(bits: Self::Bits) -> Self;
}

widths!(u8, u16, u32, u64);

/// What every access type shares: where it is, and how wide.
pub trait Access {
    /// Bytes the register takes.
    const BYTES: usize;

    /// The register at `address`.
    ///
    /// # Safety
    ///
    /// (DEVICE) `address` is mapped device memory, aligned for the register,
    /// for as long as the result lives: `register_map!` alone calls it, on a
    /// mapping `ferrix_driver::Registers` holds.
    unsafe fn at(address: usize) -> Self;
}

macro_rules! access {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name<T: Value> {
            at: usize,
            value: PhantomData<T>,
        }

        impl<T: Value> Access for $name<T> {
            const BYTES: usize = size_of::<T::Bits>();

            unsafe fn at(address: usize) -> Self {
                $name {
                    at: address,
                    value: PhantomData,
                }
            }
        }
    };
}

access!(
    /// A register that reads and writes.
    ReadWrite
);
access!(
    /// A register that only reads: writing it does not compile.
    ReadOnly
);
access!(
    /// A register that only writes: reading it does not compile.
    WriteOnly
);

impl<T: Value> ReadWrite<T> {
    /// Its value.
    #[must_use]
    pub fn read(&self) -> T {
        // SAFETY: (DEVICE) `at`'s contract: inside a mapping `Registers`
        // holds while this lives.
        T::from_bits(unsafe { T::Bits::read(self.at) })
    }

    /// Write `value`.
    pub fn write(&self, value: T) {
        // SAFETY: (DEVICE) as `read`; the mapping is writable.
        unsafe { T::Bits::write(self.at, value.into_bits()) }
    }

    /// Read, change and write back.
    pub fn modify(&self, change: impl FnOnce(T) -> T) {
        self.write(change(self.read()));
    }
}

impl<T: Value> ReadOnly<T> {
    /// Its value.
    #[must_use]
    pub fn read(&self) -> T {
        // SAFETY: (DEVICE) as `ReadWrite::read`.
        T::from_bits(unsafe { T::Bits::read(self.at) })
    }
}

impl<T: Value> WriteOnly<T> {
    /// Write `value`.
    pub fn write(&self, value: T) {
        // SAFETY: (DEVICE) as `ReadWrite::write`.
        unsafe { T::Bits::write(self.at, value.into_bits()) }
    }
}

/// A device's register layout, as `register_map!` writes it.
pub trait RegisterMap: Sized {
    /// Bytes from the first register's start to the last one's end.
    const BYTES: usize;

    /// The registers at `base`.
    ///
    /// # Safety
    ///
    /// (DEVICE) `base` is mapped device memory of at least [`Self::BYTES`],
    /// for as long as the result lives.
    unsafe fn at(base: usize) -> Self;
}

/// A field the hardware stores as its value less one: the field takes the
/// real value, `MIN` to `MAX`.
#[derive(Clone, Copy, Debug)]
pub struct OffsetByOne<const MIN: u32, const MAX: u32>;

impl<const MIN: u32, const MAX: u32> OffsetByOne<MIN, MAX> {
    /// The bits for `value`, or `None` for a value outside `MIN..=MAX`.
    #[must_use]
    pub const fn try_encode(value: u32) -> Option<u32> {
        if MIN <= value && value <= MAX && value > 0 {
            Some(value - 1)
        } else {
            None
        }
    }

    /// The value `bits` stand for.
    #[must_use]
    pub const fn decode(bits: u32) -> u32 {
        bits + 1
    }
}

/// A value a field cannot hold, as a fallible setter refuses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange {
    /// The value refused.
    pub value: u32,
}
