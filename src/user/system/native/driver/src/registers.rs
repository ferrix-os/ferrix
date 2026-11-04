//! Typed registers: what `register_map!` lays a device's registers out as,
//! and what `bitfields!` names their values with.
//!
//! A register is [`ReadWrite`], [`ReadOnly`] or [`WriteOnly`], so a write to a
//! register that only reads does not compile; its value is a `bitfields!`
//! type or a plain integer. A field that the hardware stores encoded takes
//! the real value and encodes it: [`OffsetByOne`] for a field that holds
//! "the value less one", so a driver writes `.frame_size(8)` for eight-bit
//! frames, never 7.
//!
//! The types are `ferrix-registers`', re-exported here; this module adds
//! the mapping of a device's aperture they are laid over, [`Registers`].

use core::ops::Deref;

use ferrix_native_abi::types::IoMappingSpec;
pub use ferrix_registers::{
    Access, OffsetByOne, OutOfRange, ReadOnly, ReadWrite, RegisterMap, Value, Width, WriteOnly,
};
use ferrix_rt::Kernel;
use ferrix_rt::native::device::{Device, IoMapping};

use crate::model::{Errno, Result};

/// A device's registers, mapped: the map's typed fields, through `Deref`.
#[derive(Debug)]
pub struct Registers<M: RegisterMap> {
    _mapping: IoMapping<Kernel>,
    map: M,
}

impl<M: RegisterMap> Registers<M> {
    /// Map `device`'s aperture `index` and lay `M` over it.
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such aperture, `EFAULT` for one shorter than `M`,
    /// `EIO` when the kernel refuses the mapping.
    pub fn map(device: &Device<Kernel>, index: usize) -> Result<Self> {
        let aperture = device.aperture(index).map_err(|_| Errno::ENXIO)?;
        if (aperture.len as usize) < M::BYTES {
            return Err(Errno::EFAULT);
        }
        let mapping = device
            .io_mapping(IoMappingSpec {
                phys: aperture.phys,
                len: aperture.len,
            })
            .map_err(|_| Errno::EIO)?;
        let base = mapping.map(None).map_err(|_| Errno::EIO)?;
        // SAFETY: (DEVICE) `base` is the mapping of the whole aperture,
        // which is at least `M::BYTES`, kept alive beside the map.
        let map = unsafe { M::at(base) };
        Ok(Registers {
            _mapping: mapping,
            map,
        })
    }
}

impl<M: RegisterMap> Deref for Registers<M> {
    type Target = M;

    fn deref(&self) -> &M {
        &self.map
    }
}
