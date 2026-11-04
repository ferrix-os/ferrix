//! Generic device-tree nodes (`docs/DRIVER-MODEL.md` §3): matched by
//! `compatible` against [`super::GENERIC_DRIVERS`], and admitted only when
//! the configuration may drive a node the kernel depends on (D4) and the
//! node gives everything its driver's requirement table asks for (D13).
//!
//! The node's identity record -- its `compatible` list, path, references
//! and names -- is read through `device_config_read` on the handle devmgr
//! keeps, four bytes at a time, as the kernel offers it
//! (`DEVICE_TREE_GENERIC`). The driver's table is read out of its image's
//! [`requirements::SECTION`]; an image without one is not started, since
//! devmgr then cannot say what the node must give.

use ferrix_devmgr_proto::requirements::{self, Offered, Table};
use ferrix_fdt::identity::{RECORD_BYTES, Record};
use ferrix_native_abi::types::DeviceInfo;
use ferrix_rt::Kernel;
use ferrix_rt::log::Level;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::vmo::Vmo;

use super::{GENERIC_DRIVERS, Kind, MAX_DRIVERS, NAME_BYTES, driver_name, image_index, say};

/// The node's identity record, read into `bytes` through `keep`.
fn read<'b>(keep: &Device<Kernel>, info: &DeviceInfo, bytes: &'b mut [u8]) -> Option<Record<'b>> {
    let length = usize::try_from(info.class).ok()?.min(bytes.len());
    for at in (0..length).step_by(4) {
        let word = keep.config_read(u16::try_from(at).ok()?, 4).ok()?;
        let slot = bytes.get_mut(at..(at + 4).min(length))?;
        let word = word.to_le_bytes();
        slot.copy_from_slice(word.get(..slot.len())?);
    }
    Record::parse(bytes.get(..length)?).ok()
}

/// The driver's requirement table, read out of `image` into `bytes`.
fn table<'b>(image: &Vmo<Kernel>, bytes: &'b mut [u8]) -> Option<Table<'b>> {
    let (at, length) = requirements::find_section(
        |offset, buffer| image.read(buffer, offset).is_ok(),
        requirements::SECTION,
    )?;
    let length = usize::try_from(length).ok()?;
    let slot = bytes.get_mut(..length)?;
    image.read(slot, at).ok()?;
    Table::parse(slot).ok()
}

/// The image of the driver for the generic node `info` describes, if the
/// table has a row for one of its compatibles, the initramfs carries that
/// driver, and the node is admitted for it -- with its kind and its place
/// among the names. Each refusal is said on its line, naming the node and
/// what it lacks.
pub(crate) fn driver_for<'a>(
    info: &DeviceInfo,
    keep: &Device<Kernel>,
    names: &[[u8; NAME_BYTES]; MAX_DRIVERS],
    drivers: usize,
    images: &'a [Option<Vmo<Kernel>>; MAX_DRIVERS],
    outside: bool,
) -> Option<(&'a Vmo<Kernel>, Kind, u16)> {
    let mut bytes = [0u8; RECORD_BYTES];
    let record = read(keep, info, &mut bytes)?;
    let (row, matched, wanted) = record.compatible().find_map(|compatible| {
        GENERIC_DRIVERS
            .iter()
            .position(|(binding, _, _)| *binding == compatible)
            .and_then(|row| Some((row, compatible, GENERIC_DRIVERS.get(row)?.1)))
    })?;
    let driver = image_index(names, drivers, wanted)?;
    let image = images.get(usize::from(driver)).and_then(Option::as_ref)?;
    let name = driver_name(names, driver);
    let path = record.path();
    say(
        Level::Info,
        format_args!("devmgr   {name} {path}: matched by compatible {matched}"),
    );
    // D4: a node the kernel depends on -- the console's clock, say -- is
    // never handed to a driver inside the claim.
    if record.kernel_depends() && !outside {
        say(
            Level::Notice,
            format_args!(
                "devmgr   {name} {path} not started: the kernel depends on the node \
                 (KERNEL_DEPENDS), and this configuration is inside the claim"
            ),
        );
        return None;
    }
    let mut table_bytes = [0u8; requirements::MAX_BYTES];
    let Some(table) = table(image, &mut table_bytes) else {
        say(
            Level::Notice,
            format_args!(
                "devmgr   {name} {path} not started: its image carries no requirement table"
            ),
        );
        return None;
    };
    let offered = Offered {
        record,
        apertures: info.apertures,
        vectors: info.vectors,
    };
    if let Err(unmet) = requirements::check(&table, &offered) {
        say(
            Level::Notice,
            format_args!("devmgr   {name} {path} not started: it needs {unmet}"),
        );
        return None;
    }
    Some((image, Kind::Generic(row), driver))
}
