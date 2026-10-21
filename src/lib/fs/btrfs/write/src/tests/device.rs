//! The write path's mapping of its device's failures (`crate::device`).

extern crate std;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::volume::{Device, DeviceError, ReadKind};

use crate::device::{self, Asked, device_error};
use crate::{Error, WriteDevice};

/// Every kind of device failure, in the order the table below lists them.
const ALL: [DeviceError; 4] = [
    DeviceError::OutOfMemory,
    DeviceError::Read { at: 7 },
    DeviceError::Write { at: 7 },
    DeviceError::Flush,
];

/// A device that fails every call with `error`, reporting its own address
/// (deliberately not the one asked for).
struct Refusing(DeviceError);

impl Device for Refusing {
    fn read_at(&mut self, _: u64, _: &mut [u8], _: ReadKind) -> Result<(), DeviceError> {
        Err(self.0)
    }
}

impl WriteDevice for Refusing {
    fn write_at(&mut self, _: u64, _: &[u8]) -> Result<(), DeviceError> {
        Err(self.0)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        Err(self.0)
    }
}

/// What the write path must make of each failure, per operation: out of
/// memory as itself everywhere; a read's failure `DeviceRead` at the
/// address asked; a write's `DeviceWrite` there, or `Flush` when the flush
/// of a durable write failed; a flush's `Flush`.
fn wanted(asked: Asked, error: DeviceError) -> Error {
    match (asked, error) {
        (_, DeviceError::OutOfMemory) => Error::OutOfMemory,
        (Asked::Read(physical), _) => Error::Volume(BtrfsError::DeviceRead { physical }),
        (Asked::Write(_), DeviceError::Flush) => Error::Flush,
        (Asked::Write(physical), _) => Error::DeviceWrite { physical },
        (Asked::Flush, _) => Error::Flush,
    }
}

/// Every device failure from every operation, through the mapping and
/// through each of the calls the write path makes.
///
/// Verifies: L.btrfs.118
#[test]
fn every_device_failure_maps_to_the_write_paths_error_at_the_address_asked() {
    let at = 0x1234_5000;
    for error in ALL {
        for asked in [Asked::Read(at), Asked::Write(at), Asked::Flush] {
            assert_eq!(
                device_error(asked, error),
                wanted(asked, error),
                "{asked:?}, {error:?}"
            );
        }
        let mut buf = [0u8; 16];
        let mut refusing = Refusing(error);
        assert_eq!(
            device::read(&mut refusing, at, &mut buf, ReadKind::Data),
            Err(wanted(Asked::Read(at), error)),
            "read, {error:?}"
        );
        assert_eq!(
            device::write(&mut refusing, at, &buf),
            Err(wanted(Asked::Write(at), error)),
            "write, {error:?}"
        );
        assert_eq!(
            device::write_durable(&mut refusing, at, &buf),
            Err(wanted(Asked::Write(at), error)),
            "write_durable, {error:?}"
        );
        assert_eq!(
            device::flush(&mut refusing),
            Err(wanted(Asked::Flush, error)),
            "flush, {error:?}"
        );
    }
}
