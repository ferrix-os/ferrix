//! Fuzz the btrfs read path with images `mkfs.btrfs` wrote and the fuzzer bent.
//!
//! From stage 11 a disk image is parsed in ring 0, and a disk is whatever was
//! plugged in. Starting from nothing, a fuzzer would spend its whole budget
//! failing the superblock magic. So each run starts from one of the real
//! images in `src/lib/fs/btrfs/format/testdata/` and applies a short list of edits the input
//! describes, which puts every run a few bytes away from a filesystem that
//! mounts.
//!
//! # Resealing
//!
//! Almost any edit to a node breaks its CRC-32C, and a reader that checks
//! checksums then never looks further. When the input's first byte asks for it,
//! every edited block that carries the volume's fsid — every tree node, and
//! the superblock — is re-checksummed after editing. That is the case that
//! matters: a hostile image checksums correctly, and it is the walker, the
//! item parsers and the extent arithmetic that have to hold.
//!
//! # The properties
//!
//! * Nothing panics and nothing hangs (libFuzzer's timeout is the check for
//!   the second), whatever the image.
//! * **A walk is ordered.** Keys come out of a walk strictly ascending, which
//!   is what makes the continuation argument in `volume.rs` terminate.
//! * **A read is bounded.** It never reports more bytes than were asked for,
//!   and never more in total than the inode's size.
//!
//! # Speed
//!
//! CI gives each target thirty seconds, so runs per second is coverage. The
//! four images are unpacked once, a run copies only the blocks it edits, and
//! the node, extent and zstd buffers are allocated once per thread.

#![no_main]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::OnceLock;

use ferrix_btrfs::compress::MAX_UNCOMPRESSED;
use ferrix_btrfs::compress::zstd::Workspace;
use ferrix_btrfs::fs::{ExtentBuffers, ReadBuffers, Subvolume};
use ferrix_btrfs::items::{INODE_ITEM_KEY, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use ferrix_btrfs::superblock::PRIMARY_OFFSET;
use ferrix_btrfs::volume::{Device, ReadKind, Volume};
use ferrix_btrfs::{BtrfsError, BtrfsKey, ChunkMapEntry, crc32c};
use libfuzzer_sys::fuzz_target;

const PACKED: [&[u8]; 4] = [
    include_bytes!("../../../lib/fs/btrfs/format/testdata/none.img.packed"),
    include_bytes!("../../../lib/fs/btrfs/format/testdata/zlib.img.packed"),
    include_bytes!("../../../lib/fs/btrfs/format/testdata/lzo.img.packed"),
    include_bytes!("../../../lib/fs/btrfs/format/testdata/zstd.img.packed"),
];
const BLOCK: usize = 4096;
const IMAGE_SIZE: u64 = 128 * 1024 * 1024;
/// Caps that keep one run fast without hiding a hang: a walk that is cut off
/// here would still have been caught by the ordering assertion.
const MAX_INODES: usize = 256;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const PIECE: usize = 64 * 1024;

type Blocks = BTreeMap<u64, [u8; BLOCK]>;

/// The four images, unpacked on first use.
fn bases() -> &'static [Blocks; 4] {
    static BASES: OnceLock<[Blocks; 4]> = OnceLock::new();
    BASES.get_or_init(|| PACKED.map(unpack))
}

fn unpack(packed: &[u8]) -> Blocks {
    packed
        .chunks_exact(8 + BLOCK)
        .filter_map(|record| {
            let (offset, block) = record.split_at(8);
            Some((
                u64::from_le_bytes(offset.try_into().ok()?),
                block.try_into().ok()?,
            ))
        })
        .collect()
}

/// A base image with this run's edits laid over it.
struct Image {
    base: &'static Blocks,
    edited: Blocks,
}

impl Image {
    fn block(&self, base: u64) -> Option<&[u8; BLOCK]> {
        self.edited.get(&base).or_else(|| self.base.get(&base))
    }

    /// Apply the input's edits, returning the blocks they touched.
    fn edit(&mut self, mut input: &[u8]) -> Vec<u64> {
        let offsets: Vec<u64> = self.base.keys().copied().collect();
        let mut touched = Vec::new();
        while let [kind, b0, b1, at0, at1, rest @ ..] = input {
            let ordinal = usize::from(u16::from_le_bytes([*b0, *b1])) % offsets.len().max(1);
            let Some(&base) = offsets.get(ordinal) else {
                return touched;
            };
            let at = usize::from(u16::from_le_bytes([*at0, *at1])) % BLOCK;
            let width = [1, 1, 4, 8][usize::from(kind & 3)];
            let Some((value, tail)) = rest.split_at_checked(width) else {
                return touched;
            };
            let original = self.base.get(&base).copied().unwrap_or([0; BLOCK]);
            let block = self.edited.entry(base).or_insert(original);
            for (i, byte) in value.iter().enumerate() {
                if let Some(slot) = block.get_mut(at + i) {
                    *slot = if kind & 3 == 0 { *slot ^ byte } else { *byte };
                }
            }
            touched.push(base);
            input = tail;
        }
        touched
    }

    /// Re-checksum every touched block that is the superblock or a node of
    /// this filesystem. The fixtures use 4 KiB nodes, so a node is a block.
    fn reseal(&mut self, touched: &[u64]) {
        let Some(fsid) = self
            .block(PRIMARY_OFFSET)
            .map(|sb| <[u8; 16]>::try_from(&sb[32..48]).unwrap_or_default())
        else {
            return;
        };
        for base in touched {
            if let Some(block) = self.edited.get_mut(base) {
                if *base == PRIMARY_OFFSET || block[32..48] == fsid {
                    let sum = crc32c(&block[32..]);
                    block[..4].copy_from_slice(&sum.to_le_bytes());
                }
            }
        }
    }
}

impl Device for Image {
    fn read_at(&mut self, physical: u64, buf: &mut [u8], _kind: ReadKind) -> Result<(), BtrfsError> {
        if physical
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > IMAGE_SIZE)
        {
            return Err(BtrfsError::DeviceRead { physical });
        }
        let mut done = 0;
        while done < buf.len() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let within = (at - base) as usize;
            let len = (BLOCK - within).min(buf.len() - done);
            let dest = &mut buf[done..done + len];
            match self.block(base) {
                Some(block) => dest.copy_from_slice(&block[within..within + len]),
                None => dest.fill(0),
            }
            done += len;
        }
        Ok(())
    }
}

/// Buffers allocated once per thread rather than once per run.
struct Scratch {
    node: Vec<u8>,
    compressed: Vec<u8>,
    plain: Vec<u8>,
    zstd: Vec<u8>,
    csum_node: Vec<u8>,
    piece: Vec<u8>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch {
        node: vec![0; 65536],
        compressed: vec![0; MAX_UNCOMPRESSED],
        plain: vec![0; MAX_UNCOMPRESSED],
        zstd: vec![0; Workspace::SIZE],
        csum_node: vec![0; 65536],
        piece: vec![0; PIECE],
    });
}

/// Read one file or symlink up to the cap, checking every count.
fn read_file(
    sub: &Subvolume<'_, &mut [ChunkMapEntry; 32]>,
    image: &mut Image,
    ino: u64,
    size: u64,
    node: &mut [u8],
    piece: &mut [u8],
    buffers: &mut ReadBuffers<impl ExtentBuffers>,
) {
    let mut total = 0u64;
    while total < size.min(MAX_FILE_BYTES) {
        let Ok(n) = sub.read(image, ino, total, piece, node, buffers) else {
            break;
        };
        assert!(n <= piece.len(), "a read reports no more than it was given");
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    assert!(total <= size, "a file reads no further than its size");
}

/// Read every file, list every directory, and follow every name back.
fn exercise(
    sub: &Subvolume<'_, &mut [ChunkMapEntry; 32]>,
    image: &mut Image,
    inodes: &[u64],
    scratch: &mut Scratch,
) {
    let Scratch {
        node,
        compressed,
        plain,
        zstd,
        csum_node,
        piece,
    } = scratch;
    let Ok(mut buffers) = ReadBuffers::new((
        &mut compressed[..],
        &mut plain[..],
        &mut zstd[..],
        &mut csum_node[..],
    )) else {
        return;
    };
    for &ino in inodes {
        let Ok(Some(inode)) = sub.inode(image, ino, node) else {
            continue;
        };
        match inode.mode & S_IFMT {
            S_IFREG | S_IFLNK => read_file(sub, image, ino, inode.size, node, piece, &mut buffers),
            S_IFDIR => {
                let mut names = Vec::new();
                let _ = sub.read_dir(image, ino, 2, node, |entry| {
                    names.push(entry.name.to_vec());
                    if names.len() < 64 {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(())
                    }
                });
                for name in names {
                    // A corrupt image may list a name its hash index lacks, so
                    // only the absence of a panic is checked here.
                    let _ = sub.lookup(image, ino, &name, node);
                }
            }
            _ => {}
        }
    }
}

fuzz_target!(|input: &[u8]| {
    let Some((&selector, edits)) = input.split_first() else {
        return;
    };
    let mut image = Image {
        base: &bases()[usize::from(selector & 3)],
        edited: Blocks::new(),
    };
    let touched = image.edit(edits);
    if selector & 0x80 != 0 {
        image.reseal(&touched);
    }

    SCRATCH.with_borrow_mut(|scratch| {
        let mut chunks = [ChunkMapEntry::EMPTY; 32];
        let Ok(volume) = Volume::open(&mut image, &mut chunks, &mut scratch.node) else {
            return;
        };

        let mut inodes = Vec::new();
        let mut previous: Option<BtrfsKey> = None;
        let _ = volume.walk(
            &mut image,
            volume.fs_tree(),
            BtrfsKey::MIN,
            &mut scratch.node,
            |_, item| {
                assert!(
                    previous.is_none_or(|p| p < item.key),
                    "a walk yields keys in strictly ascending order"
                );
                previous = Some(item.key);
                if item.key.item_type == INODE_ITEM_KEY {
                    inodes.push(item.key.objectid);
                }
                Ok(if inodes.len() < MAX_INODES {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                })
            },
        );

        let sub = volume.default_subvolume();
        exercise(&sub, &mut image, &inodes, scratch);
    });
});
