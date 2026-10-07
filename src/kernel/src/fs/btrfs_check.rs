//! Stage 11's exit: a btrfs image made by real `mkfs.btrfs`, on a disk a
//! ring-3 driver serves, mounted through the kernel's own mount path and
//! read back against what the host wrote.
//!
//! `xtask` attaches the `none` fixture from `src/lib/fs/btrfs/testdata` as the
//! second virtio-blk disk, and the driver check starts a driver for every
//! virtio-blk function, so by the time this runs the fixture is `vdb` in the
//! registry. The check mounts it read-only at `/mnt` exactly as `mount -t
//! btrfs -o ro /dev/vdb /mnt` would, then walks the fixture's manifest: every
//! file read whole through the VFS, its size and CRC-32C compared with the
//! manifest's, which `tools/common/gen/gen-btrfs-fixtures.py` computed from the bytes
//! it gave `mkfs.btrfs`; every directory found to be one; every link's target
//! read and compared the same way. The mount is left in place, so a program
//! in the initramfs can read the tree too, which `test-vfs` does.
//!
//! What is read comes off the disk through the whole stack: the ring-3
//! driver's virtio queue, the block ring, the registry's `BlockDevice`, the
//! btrfs volume reader with its checksums, and the inode's VMO pages filled
//! from the page source. A byte wrong anywhere is a CRC that differs.
//!
//! Every file is then mapped too, as glibc's loader maps a library: opened
//! and `mmap`ped privately by a process made for the check, and its bytes
//! read back through the mapping, each page faulted in from the same page
//! cache, against the same CRC. A read-only btrfs answered `ENODEV` to every
//! `mmap` until 2026-10-07, so no program could run from one.

use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_btrfs::crc32c;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType};

use crate::fs;
use crate::interfaces::block_ring::VIRTIO_BLK_MAJOR;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::{fd, process, uaccess};

/// The fixture's manifest: `kind hex(path) size crc32c` per line.
const MANIFEST: &str = include_str!("../../../lib/fs/btrfs/testdata/manifest.txt");

/// Where the disk is mounted.
const MOUNT_POINT: &[u8] = b"/mnt";

/// The fixture's disk: the second virtio-blk function, `vdb`.
const DISK_INDEX: u32 = 1;

/// What the check found.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) files: u32,
    pub(crate) directories: u32,
    pub(crate) links: u32,
    /// Bytes of file data read back.
    pub(crate) bytes: u64,
    /// Why nothing was checked: no second disk on this machine.
    pub(crate) skipped: Option<&'static str>,
}

/// Mount the fixture's disk and read its tree back.
///
/// # Errors
///
/// What did not match, as a sentence; the manifest line is printed first.
pub(crate) fn run() -> Result<Report, &'static str> {
    let rdev = makedev(VIRTIO_BLK_MAJOR, DISK_INDEX * 16);
    if fs::devfs::block_device(rdev).is_none() {
        return Ok(Report {
            files: 0,
            directories: 0,
            links: 0,
            bytes: 0,
            skipped: Some("no second disk is served"),
        });
    }
    let ns = fs::namespace();
    let ctx = ns.context();
    match ns.mkdir(&ctx, None, MOUNT_POINT, 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("the mount point could not be made"),
    }
    let at = ns
        .resolve(&ctx, None, MOUNT_POINT, true)
        .map_err(|_| "the mount point could not be resolved")?;
    let volume = fs::btrfs::mount(rdev).map_err(|_| "the btrfs disk would not mount")?;
    let _ = ns
        .mount(volume, &at)
        .map_err(|_| "the volume could not be mounted at /mnt")?;

    let mut report = Report {
        files: 0,
        directories: 0,
        links: 0,
        bytes: 0,
        skipped: None,
    };
    let mut mapped = Vec::new();
    for line in MANIFEST
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        let mut fields = line.split(' ');
        let (Some(kind), Some(hex), Some(size), Some(crc)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err("a manifest line is short");
        };
        let size: u64 = size
            .parse()
            .map_err(|_| "a manifest size is not a number")?;
        let crc = u32::from_str_radix(crc, 16).map_err(|_| "a manifest crc is not hex")?;
        let mut path = MOUNT_POINT.to_vec();
        path.push(b'/');
        path.extend(unhex(hex)?);
        let outcome = match kind {
            "file" => {
                let data = fs::read_file(&ctx, None, &path)
                    .map_err(|_| "a file of the fixture could not be read")?;
                report.bytes = report.bytes.saturating_add(data.len() as u64);
                report.files += 1;
                mapped.push((path.clone(), size, crc));
                verify(
                    &data,
                    size,
                    crc,
                    "a file's bytes differ from what the host wrote",
                )
            }
            "dir" => {
                let at = ns
                    .resolve(&ctx, None, &path, true)
                    .map_err(|_| "a directory of the fixture is missing")?;
                let stat = ns
                    .stat(&at)
                    .map_err(|_| "a directory of the fixture cannot be stat'ed")?;
                report.directories += 1;
                if stat.metadata.kind == FileType::Directory {
                    Ok(())
                } else {
                    Err("a directory of the fixture is not a directory")
                }
            }
            "link" => {
                let target = ns
                    .read_link(&ctx, None, &path)
                    .map_err(|_| "a link of the fixture could not be read")?;
                report.links += 1;
                verify(
                    &target,
                    size,
                    crc,
                    "a link's target differs from what the host wrote",
                )
            }
            _ => Err("a manifest line has an unknown kind"),
        };
        if let Err(problem) = outcome {
            crate::console::println!("  btrfs    manifest line: {line}");
            return Err(problem);
        }
    }
    map_each(&mapped)?;
    Ok(report)
}

/// Whether `data` is `size` bytes with CRC-32C `crc`.
fn verify(data: &[u8], size: u64, crc: u32, problem: &'static str) -> Result<(), &'static str> {
    if data.len() as u64 != size {
        return Err("a size differs from what the host wrote");
    }
    if crc32c(data) != crc {
        return Err(problem);
    }
    Ok(())
}

/// The bytes a manifest's hex path spells.
fn unhex(hex: &str) -> Result<Vec<u8>, &'static str> {
    if !hex.len().is_multiple_of(2) {
        return Err("a manifest path is odd hex");
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            core::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or("a manifest path is not hex")
        })
        .collect()
}

/// Map each of `files` -- a path, its size and its CRC-32C -- privately and
/// read only, from a process made for the check, and compare the bytes read
/// through the mapping. An empty file is opened and not mapped: Linux maps
/// none either.
fn map_each(files: &[(Vec<u8>, u64, u32)]) -> Result<(), &'static str> {
    use ferrix_linux_abi::nr::Syscall;
    use ferrix_linux_abi::types::{
        AT_FDCWD, MAP_ANONYMOUS, MAP_PRIVATE, O_RDONLY, PROT_READ, PROT_WRITE,
    };
    const CWD: u64 = AT_FDCWD as i64 as u64;
    let request = |fd: i64, len: u64, prot: u32, flags: u32| MmapRequest {
        addr: 0,
        len,
        prot,
        flags,
        fd,
        offset: 0,
        unit: OffsetUnit::Bytes,
    };
    let process =
        process::new_for_check().map_err(|_| "could not make a process to map the fixture")?;
    let page = memory::sys_mmap(
        &process,
        &request(
            -1,
            PAGE_SIZE,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS | MAP_PRIVATE,
        ),
    )
    .ok()
    .and_then(|at| u64::try_from(at).ok())
    .ok_or("a page to stage a path in was refused")?;
    for (path, size, crc) in files {
        let mut staged = path.clone();
        staged.push(0);
        if staged.len() as u64 > PAGE_SIZE {
            continue;
        }
        uaccess::copy_to_user(process.space(), page, &staged)
            .map_err(|_| "could not stage a fixture path")?;
        let descriptor = crate::syscall::check::call_by_number(
            &process,
            Syscall::Openat,
            [CWD, page, u64::from(O_RDONLY), 0, 0, 0],
        )
        .ok()
        .and_then(|fd| i32::try_from(fd).ok())
        .ok_or("a file of the fixture would not open for mapping")?;
        let outcome = (|| {
            if *size == 0 {
                return Ok(());
            }
            let at = memory::sys_mmap(
                &process,
                &request(i64::from(descriptor), *size, PROT_READ, MAP_PRIVATE),
            )
            .map_err(|_| "a file of the read-only btrfs could not be mapped")?;
            let at = u64::try_from(at).map_err(|_| "mmap returned an impossible address")?;
            let mut data = Vec::new();
            data.try_reserve_exact(usize::try_from(*size).unwrap_or(usize::MAX))
                .map_err(|_| "no memory to read a mapping back into")?;
            data.resize(usize::try_from(*size).unwrap_or(0), 0_u8);
            let read = uaccess::copy_from_user(process.space(), at, &mut data);
            let _ = memory::sys_munmap(&process, at, *size);
            read.map_err(|_| "a mapping of a fixture file could not be read")?;
            verify(
                &data,
                *size,
                *crc,
                "a file's bytes through a mapping differ from what the host wrote",
            )
        })();
        let _ = fd::sys_close(&process, descriptor);
        if let Err(problem) = outcome {
            crate::console::println!(
                "  btrfs    mapped: {}",
                alloc::string::String::from_utf8_lossy(path)
            );
            return Err(problem);
        }
    }
    let _ = memory::sys_munmap(&process, page, PAGE_SIZE);
    Ok(())
}
