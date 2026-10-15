//! Just enough UEFI to load a kernel.
//!
//! Hand-written rather than pulled from a crate, for the reason the workspace
//! manifest gives: this is the code that runs before anything else exists, and
//! the subset it needs — a memory map, a file, a framebuffer and a way to say
//! goodbye to firmware — is small enough to read in one sitting.
//!
//! Every structure here is `repr(C)` and its fields are declared **in the order
//! the specification gives, including the ones we never call**. A missing
//! function pointer does not fail to compile; it silently shifts every field
//! after it, and the symptom is calling `free_pool` when you meant
//! `get_memory_map`.
//!
//! Reference: UEFI Specification 2.10.

pub(crate) mod protocols;
pub(crate) mod tables;

use core::ffi::c_void;

/// An opaque firmware object.
pub(crate) type Handle = *mut c_void;

/// A UEFI return code.
///
/// Zero is success. The top bit marks an error, so a status is checked with
/// [`Status::is_success`] rather than by sign — on a 64-bit target every error
/// code is a very large positive number, not a negative one.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Status(pub(crate) usize);

impl Status {
    /// Set in the top bit of every error code.
    const ERROR_BIT: usize = 1 << (usize::BITS - 1);

    /// What `efi_main` returns when the loader could not start the kernel.
    pub(crate) const LOAD_ERROR: Status = Status(Status::ERROR_BIT | 1);
    /// Expected, not exceptional: how `get_memory_map` reports the size it
    /// needs when asked with a zero-length buffer.
    pub(crate) const BUFFER_TOO_SMALL: Status = Status(Status::ERROR_BIT | 5);
    /// Expected, not exceptional: how opening a file that is not on the volume
    /// fails, which is how an image with no initramfs says so.
    pub(crate) const NOT_FOUND: Status = Status(Status::ERROR_BIT | 14);
    /// What a download the loader gave up waiting for is aborted with.
    pub(crate) const TIMEOUT: Status = Status(Status::ERROR_BIT | 18);

    /// True if the call succeeded.
    pub(crate) const fn is_success(self) -> bool {
        self.0 == 0
    }

    /// True if the top bit is set.
    pub(crate) const fn is_error(self) -> bool {
        self.0 & Status::ERROR_BIT != 0
    }

    /// The error number without the top bit, for printing.
    pub(crate) const fn code(self) -> usize {
        self.0 & !Status::ERROR_BIT
    }
}

/// A UEFI GUID, which identifies a protocol or a configuration table.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Guid {
    pub(crate) data1: u32,
    pub(crate) data2: u16,
    pub(crate) data3: u16,
    pub(crate) data4: [u8; 8],
}

impl Guid {
    /// Spell a GUID the way the specification prints it.
    pub(crate) const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
        Guid {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// The header every UEFI table begins with.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct TableHeader {
    pub(crate) signature: u64,
    pub(crate) revision: u32,
    pub(crate) header_size: u32,
    pub(crate) crc32: u32,
    pub(crate) reserved: u32,
}
