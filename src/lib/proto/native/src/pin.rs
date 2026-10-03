//! Pinning a VMO's pages into a device's IOMMU domain.
//!
//! A driver gives its device the addresses of pages it pinned, and the
//! device's domain decides what those addresses reach. [`Device::pin`] holds a
//! range of a VMO in the domain for as long as the [`Pin`] handle is open, and
//! [`Pin::addresses`] reads back where the device sees each page. The pages are
//! not contiguous to the device, so there is one address per page.

use ferrix_native_abi::nr;
use ferrix_native_abi::types::{PIN_COHERENT, PIN_CONTIGUOUS, PIN_READ_ONLY};

use crate::call::{Call, Syscall};
use crate::device::Device;
use crate::error::{Error, decode, decode_handle};
use crate::handle::{Object, OwnedHandle, object_handle, register};
use crate::vmo::Vmo;

object_handle!(
    /// Pages of a VMO pinned into a device's domain. Closing it unpins them.
    ///
    /// It carries `Rights::PIN`, which is `READ` alone: it can be neither
    /// duplicated nor sent to another process.
    Pin
);

/// What the device may do with the pages it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAccess {
    /// Read and write. Needs `WRITE` on the VMO as well as `READ`.
    ReadWrite,
    /// Read only: `PIN_READ_ONLY`. Needs `READ` on the VMO.
    ReadOnly,
    /// Read and write, and seen alike by the program and the device:
    /// `PIN_COHERENT`, for descriptors a controller polls. Covers the whole
    /// VMO, which must not be mapped until the pin is made; on a device that
    /// does not snoop the caches, its mappings then bypass them.
    Coherent,
    /// Read and write, and one run of ascending addresses: `PIN_CONTIGUOUS`,
    /// for memory a device takes as one block. The range must be
    /// uncommitted, in an anonymous VMO, and at most one buddy block long;
    /// the kernel fills it, and refuses rather than pin anything else.
    Contiguous,
}

impl PinAccess {
    /// The `options` register.
    #[must_use]
    pub const fn register(self) -> usize {
        match self {
            PinAccess::ReadWrite => 0,
            PinAccess::ReadOnly => PIN_READ_ONLY as usize,
            PinAccess::Coherent => PIN_COHERENT as usize,
            PinAccess::Contiguous => PIN_CONTIGUOUS as usize,
        }
    }
}

/// A device address as the kernel writes it: eight native-endian bytes.
///
/// Bytes rather than a `u64`, so that a buffer of them is a byte buffer of the
/// same length, which the kernel fills with no assumption about alignment and
/// a wrapper passes on without a copy or a limit. [`device_address`] reads one.
pub type DeviceAddress = [u8; 8];

/// The address `bytes` holds.
#[must_use]
pub const fn device_address(bytes: DeviceAddress) -> u64 {
    u64::from_ne_bytes(bytes)
}

/// What an address query found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Addresses {
    /// How many pages the pin holds.
    pub pages: usize,
    /// How many of their addresses were written, in page order.
    pub written: usize,
}

impl Addresses {
    /// Whether every page's address was written, rather than as many as fit.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.written == self.pages
    }
}

impl<S: Syscall> Device<S> {
    /// `vmo_pin`: pin `length` bytes of `vmo` from `offset` into this device's
    /// domain.
    ///
    /// Both are plain byte counts in registers, multiples of 4 KiB, and the
    /// range is non-empty and ends within the VMO.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for a range that is not whole pages, is empty or
    /// runs past the VMO; [`Error::AccessDenied`] without `MANAGE` on the
    /// device, `READ` on the VMO, or `WRITE` for [`PinAccess::ReadWrite`];
    /// [`Error::WrongType`]; [`Error::NoMemory`], which includes a page the
    /// device's IOMMU cannot address; [`Error::AlreadyBound`].
    pub fn pin(
        &self,
        vmo: &Vmo<S>,
        offset: usize,
        length: usize,
        access: PinAccess,
    ) -> Result<Pin<S>, Error> {
        let value = Call::new(nr::VMO_PIN)
            .value(register(self.handle()))
            .value(register(vmo.handle()))
            .value(offset)
            .value(length)
            .value(access.register())
            .make(self.syscall());
        let handle = decode_handle(value)?;
        Ok(Pin::from_owned(OwnedHandle::from_raw(
            self.syscall(),
            handle,
        )))
    }
}

impl<S: Syscall> Pin<S> {
    /// `vmo_pin_addresses`: the pinned pages' device addresses, in page order,
    /// into as many of `out` as there is room for.
    ///
    /// The kernel answers with the pin's page count whatever fits, so check
    /// [`Addresses::is_complete`] before trusting the buffer to cover the pin.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `READ`; [`Error::WrongType`].
    pub fn addresses(&self, out: &mut [DeviceAddress]) -> Result<Addresses, Error> {
        let capacity = out.len();
        let value = Call::new(nr::VMO_PIN_ADDRESSES)
            .value(register(self.handle()))
            .output(out.as_flattened_mut())
            .value(capacity)
            .make(self.syscall());
        let pages = decode(value)?;
        Ok(Addresses {
            pages,
            written: pages.min(capacity),
        })
    }
}
