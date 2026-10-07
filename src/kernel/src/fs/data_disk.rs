//! A data disk: a btrfs volume at `/data`.
//!
//! Any btrfs disk from the fourth virtio-blk function on that is neither the
//! root disk ([`super::root_disk`]) nor the home disk ([`super::home_disk`]),
//! each told apart by its label, is a data disk, and the first one is mounted writable at `/data` — inside the
//! `/` processes see, so under a btrfs root it is `/sysroot/data` in the
//! kernel's own tree. What the disk is for is its attacher's business: a
//! test's own volume under QEMU's `snapshot=on`, say, which the guest may
//! write and nobody keeps.
//!
//! It is committed with the root's committer when there is one, at every
//! `sync`, and at the kernel's own power-off. A disk that is itself
//! read-only is mounted read-only instead, and there is nothing to commit.
//!
//! A mount that fails is said and not fatal, for the reason the root disk
//! gives.

use alloc::sync::Arc;

use ferrix_sync::Once;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileSystem};

use crate::console::println;
use crate::fs;
use crate::fs::root_disk;
use crate::interfaces::block_ring::VIRTIO_BLK_MAJOR;

/// Where it is mounted, in the `/` processes see.
const MOUNT_POINT: &[u8] = b"/data";

/// The first disk looked at, `vdd`, and one past the last, `vdh`.
const FIRST: u32 = 3;
const END: u32 = 8;

/// The mounted volume, for [`sync`].
static VOLUME: Once<Arc<dyn FileSystem>> = Once::new();

/// Mount the first data disk at `/data`, if this machine has one. Called
/// after [`root_disk::switch`], so that `/data` lands in the `/` it chose.
pub(crate) fn mount() {
    let Some((index, rdev)) = (FIRST..END)
        .map(|index| (index, makedev(VIRTIO_BLK_MAJOR, index * 16)))
        .filter(|&(_, rdev)| fs::devfs::block_device(rdev).is_some())
        .find(|&(_, rdev)| {
            root_disk::root_rdev() != Some(rdev)
                && !root_disk::is_root(rdev)
                && !fs::home_disk::is_home(rdev)
        })
    else {
        return;
    };
    let name = char::from(b'a'.saturating_add(u8::try_from(index).unwrap_or(0)));
    match mount_at(rdev) {
        Ok((volume, Access::Writable)) => {
            let _ = VOLUME.call_once(|| volume);
            println!("  data     vd{name} mounted writable at /data");
        }
        Ok((_, Access::ReadOnly)) => {
            println!("  data     vd{name} is a read-only disk, mounted read-only at /data");
        }
        Err(why) => println!("  data     vd{name} is not mounted: {why}"),
    }
}

/// How the volume was mounted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    /// Writable, committed at `sync` and at power-off.
    Writable,
    /// Read-only: the disk itself is (`VIRTIO_BLK_F_RO`). A program volume
    /// attached so -- `test-chrome`'s on ARMv7-A -- is one whose clean pages
    /// the page cache may give back under pressure and read again, which it
    /// may not yet for a writable btrfs (`docs/CHROME.md` §10).
    ReadOnly,
}

/// Make `/data` and mount the volume on it: writable, or read-only when the
/// disk is, which the write path refuses with `EROFS`.
fn mount_at(rdev: u64) -> Result<(Arc<dyn FileSystem>, Access), &'static str> {
    let ns = fs::namespace();
    let ctx = root_disk::process_context();
    match ns.mkdir(&ctx, None, MOUNT_POINT, 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("/data could not be made"),
    }
    let at = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "/data could not be resolved")?;
    let read_only = fs::devfs::block_device(rdev).is_some_and(|disk| disk.read_only());
    let (volume, access) = if read_only {
        let volume = fs::btrfs::mount(rdev).map_err(|errno| match errno {
            Errno::ENXIO => "the disk went away",
            _ => "it is not a btrfs volume this kernel can read",
        })?;
        (volume, Access::ReadOnly)
    } else {
        let volume = fs::btrfs::mount_rw(rdev).map_err(|errno| match errno {
            Errno::EROFS => "the volume is one the write path will not change (read-only)",
            Errno::ENXIO => "the disk went away",
            _ => "it is not a btrfs volume this kernel can mount writable",
        })?;
        (volume, Access::Writable)
    };
    let _ = ns
        .mount(volume.clone(), &at)
        .map_err(|_| "the volume could not be mounted at /data")?;
    Ok((volume, access))
}

/// Commit `/data` now, if it is mounted.
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
