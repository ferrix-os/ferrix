//! The home disk: a btrfs volume at `/home`.
//!
//! The users' files are kept on a disk of their own, so that the system's
//! root ([`super::root_disk`]) can be made again without them: `cargo xtask
//! run --reset-root` starts the root over and leaves this disk alone, and
//! `--reset-flash` starts both over. The disk is told apart by its label,
//! as the root is, and the first virtio-blk disk from `vdd` on that carries
//! it is mounted writable at `/home` -- inside the `/` processes see, so
//! under a btrfs root it is `/sysroot/home` in the kernel's own tree. A data
//! disk ([`super::data_disk`]) never takes it.
//!
//! It is committed with the root's committer, at every `sync`, and at the
//! kernel's own power-off.
//!
//! A mount that fails is said and not fatal, for the reason the root disk
//! gives: `/home` is then the directory the system's archive carries.

use alloc::sync::Arc;

use ferrix_sync::Once;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileSystem};

use crate::console::println;
use crate::fs;
use crate::fs::root_disk;
use crate::interfaces::block_ring::VIRTIO_BLK_MAJOR;

/// The label the home volume carries: `mkfs.btrfs -L ferrix-home`.
const LABEL: &[u8] = b"ferrix-home";

/// Where it is mounted, in the `/` processes see.
const MOUNT_POINT: &[u8] = b"/home";

/// The first disk looked at, `vdd`, and one past the last, `vdh`: the data
/// disk's range.
const FIRST: u32 = 3;
const END: u32 = 8;

/// The mounted volume, for [`sync`].
static VOLUME: Once<Arc<dyn FileSystem>> = Once::new();

/// Whether the disk numbered `rdev` holds the home volume, by its label.
pub(crate) fn is_home(rdev: u64) -> bool {
    fs::btrfs::label(rdev).as_deref() == Some(LABEL)
}

/// Mount the home disk at `/home`, if this machine has one. Called after
/// [`root_disk::switch`], so that `/home` lands in the `/` it chose.
pub(crate) fn mount() {
    let Some((index, rdev)) = (FIRST..END)
        .map(|index| (index, makedev(VIRTIO_BLK_MAJOR, index * 16)))
        .filter(|&(_, rdev)| fs::devfs::block_device(rdev).is_some())
        .find(|&(_, rdev)| is_home(rdev))
    else {
        return;
    };
    let name = char::from(b'a'.saturating_add(u8::try_from(index).unwrap_or(0)));
    match mount_at(rdev) {
        Ok(volume) => {
            let _ = VOLUME.call_once(|| volume);
            println!("  home     vd{name} mounted writable at /home");
        }
        Err(why) => println!("  home     vd{name} is not mounted: {why}"),
    }
}

/// Make `/home` if it is not there, and mount the volume on it.
fn mount_at(rdev: u64) -> Result<Arc<dyn FileSystem>, &'static str> {
    let ns = fs::namespace();
    let ctx = root_disk::process_context();
    match ns.mkdir(&ctx, None, MOUNT_POINT, 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("/home could not be made"),
    }
    let at = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "/home could not be resolved")?;
    let volume = fs::btrfs::mount_rw(rdev).map_err(|errno| match errno {
        Errno::EROFS => "the volume is one the write path will not change (read-only)",
        Errno::ENXIO => "the disk went away",
        _ => "it is not a btrfs volume this kernel can mount writable",
    })?;
    let _ = ns
        .mount(volume.clone(), &at)
        .map_err(|_| "the volume could not be mounted at /home")?;
    Ok(volume)
}

/// Commit `/home` now, if it is mounted.
///
/// # Errors
///
/// What the commit said.
pub(crate) fn sync() -> Result<(), Errno> {
    match VOLUME.get() {
        Some(volume) => volume.sync(),
        None => Ok(()),
    }
}
