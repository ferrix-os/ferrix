//! SCRATCH (not for main): a large sparse volume in a host file, written
//! through the writable mount until it answers ENOSPC or the target is
//! reached. FERRIX_BTRFS_BIG_IMAGE names the image, FERRIX_BTRFS_BIG_GIB the
//! target, FERRIX_BTRFS_BIG_FILES how many files are written round-robin.

extern crate std;

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc as StdArc;

use alloc::sync::Arc;
use alloc::vec;

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::volume::{Device, ReadKind};
use ferrix_sync::SpinParker;
use ferrix_vfs::tmpfs::HeapStorage;
use ferrix_vfs::{Clock, FileSystem, NewNode, Timespec};

use crate::rw::RwBtrfs;

#[derive(Clone, Debug)]
struct FileDisk(StdArc<File>);

impl Device for FileDisk {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], _kind: ReadKind) -> Result<(), BtrfsError> {
        self.0
            .read_exact_at(buf, physical)
            .map_err(|_| BtrfsError::DeviceRead { physical })
    }
}

impl ferrix_btrfs_write::WriteDevice for FileDisk {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> ferrix_btrfs_write::Result<()> {
        self.0
            .write_all_at(data, physical)
            .map_err(|_| ferrix_btrfs_write::Error::DeviceWrite { physical })
    }

    fn flush(&mut self) -> ferrix_btrfs_write::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct Fixed;

impl Clock for Fixed {
    fn now(&self) -> Timespec {
        Timespec {
            tv_sec: 1_790_000_000,
            tv_nsec: 0,
        }
    }
}

fn notice(what: core::fmt::Arguments<'_>) {
    std::eprintln!("notice: {what}");
}

fn env(name: &str) -> Option<std::string::String> {
    std::env::var(name).ok()
}

#[test]
#[ignore = "scratch: needs FERRIX_BTRFS_BIG_IMAGE"]
fn a_big_volume_takes_what_statfs_offers() {
    let Some(path) = env("FERRIX_BTRFS_BIG_IMAGE") else {
        return;
    };
    let gib: u64 = env("FERRIX_BTRFS_BIG_GIB").map_or(12, |v| v.parse().unwrap());
    let files: usize = env("FERRIX_BTRFS_BIG_FILES").map_or(1, |v| v.parse().unwrap());
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let disk = FileDisk(StdArc::new(file));
    let fs = RwBtrfs::mount(
        disk,
        0x0800_0010,
        Arc::new(HeapStorage::new(1 << 40)),
        Arc::new(Fixed),
        &SpinParker,
        notice,
    )
    .expect("mounts for writing");
    let root = fs.root();
    let dir = root.create(b"big", NewNode::Directory, 0o755).unwrap();
    let handles: vec::Vec<_> = (0..files)
        .map(|i| {
            dir.create(std::format!("part{i}").as_bytes(), NewNode::Regular, 0o644)
                .unwrap()
        })
        .collect();
    let piece = vec![0x5au8; 1 << 20];
    let mut offsets = vec![0u64; files];
    let mut total = 0u64;
    let target = gib << 30;
    let start = fs.statfs();
    std::eprintln!(
        "start: blocks {} available {} ({} GiB)",
        start.blocks,
        start.blocks_available,
        start.blocks_available * 4096 >> 30
    );
    let mut round = 0usize;
    while total < target {
        let i = round % files;
        round += 1;
        match handles[i].write_at(offsets[i], &piece, false) {
            Ok((n, _)) => {
                offsets[i] += n as u64;
                total += n as u64;
            }
            Err(error) => {
                let s = fs.statfs();
                std::eprintln!(
                    "ENOSPC-ish {error:?} after {total} bytes ({} MiB); statfs available {} ({} MiB)",
                    total >> 20,
                    s.blocks_available,
                    s.blocks_available * 4096 >> 20
                );
                panic!("refused after {} MiB: {error:?}", total >> 20);
            }
        }
        if total % (1 << 30) == 0 {
            let s = fs.statfs();
            std::eprintln!(
                "{} GiB written; statfs available {} MiB",
                total >> 30,
                s.blocks_available * 4096 >> 20
            );
        }
    }
    fs.sync().unwrap();
    std::eprintln!("done: {} GiB", total >> 30);
}
