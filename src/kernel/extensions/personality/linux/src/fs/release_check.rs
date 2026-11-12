//! The release's boot check (`docs/RAM-RELEASE.md` §2, the `release` line,
//! FX-1203): [`super::release`] run between a look before it and a look
//! after, on every boot whose checks run -- the test boots, which keep their
//! tmpfs, and the boots with a root disk (`test-restart --boot blk`,
//! `test-disk-offline`), which give it back.
//!
//! The release happens once a boot, so it cannot be tried on a fixture: the
//! check is around the real one, and stops the machine if it was not as the
//! requirements say.

use alloc::vec::Vec;

use ferrix_bootinfo::{MemKind, MemRegion, PAGE_SIZE};
use ferrix_vfs::initramfs;
use ferrix_vfs::{FileType, OpenFile, OpenFlags};

use super::{Released, namespace, release, root_disk, spared};
use crate::console::println;
use crate::mm;
use crate::panic::catalog;
use crate::syscall::program::ProgramFile;

/// How much of the file held across the release is compared: its first
/// 64 KiB, which is every page of most of the system's files and bounds the
/// check's time for the rest.
const COMPARED: u64 = 64 * 1024;

/// What the check compares in pieces of, on its stack.
const PIECE: usize = 512;

/// FNV-1a's 64-bit offset basis and prime.
const FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// `hash` carried over `bytes`, FNV-1a.
fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// A file of the tmpfs held open, and the object its mapping maps, across
/// the release: what a process that had it open or mapped keeps.
struct Held {
    /// The open file and its VMO.
    program: ProgramFile,
    /// The bytes compared, [`COMPARED`] at most.
    len: u64,
    /// Their fingerprint, read through the file before the release.
    through_file: u64,
    /// Their fingerprint, read from the VMO before the release.
    through_object: u64,
}

impl Held {
    /// The first regular file of `names` that is not kept anyway, opened,
    /// and its fingerprints both ways.
    fn first_of(names: &[Vec<u8>]) -> Result<Held, &'static str> {
        let ns = namespace();
        let ctx = ns.context();
        let flags = OpenFlags {
            read: true,
            ..OpenFlags::default()
        };
        for name in names.iter().filter(|name| !kept(name)) {
            let Ok(at) = ns.resolve(&ctx, Some(&ctx.root), name, false) else {
                continue;
            };
            let Ok(stat) = ns.stat(&at) else {
                continue;
            };
            if stat.metadata.kind != FileType::Regular || stat.metadata.size == 0 {
                continue;
            }
            let file = OpenFile::new(at, &flags).map_err(|_| "a file of the tmpfs did not open")?;
            let program =
                ProgramFile::open(file).map_err(|_| "a file of the tmpfs did not read")?;
            let len = program.len().min(COMPARED);
            let through_file = through_file(&program, len)?;
            let through_object = through_object(&program, len)?;
            if through_file != through_object {
                return Err("a file of the tmpfs reads otherwise through its VMO");
            }
            return Ok(Held {
                program,
                len,
                through_file,
                through_object,
            });
        }
        Err("the tmpfs holds no file of the archive to hold across the release")
    }
}

/// The first `len` bytes of `program`'s file, read through it.
fn through_file(program: &ProgramFile, len: u64) -> Result<u64, &'static str> {
    let mut hash = FNV_BASIS;
    let mut piece = [0_u8; PIECE];
    let mut at = 0;
    while at < len {
        let take = (len - at).min(PIECE as u64) as usize;
        let slot = piece.get_mut(..take).ok_or("a piece out of range")?;
        program
            .read_exact_at(at, slot)
            .map_err(|_| "the file held across the release does not read")?;
        hash = fnv(hash, slot);
        at += take as u64;
    }
    Ok(hash)
}

/// The first `len` bytes of `program`'s file, read from the VMO a mapping
/// of it maps.
fn through_object(program: &ProgramFile, len: u64) -> Result<u64, &'static str> {
    let (vmo, offset) = program
        .object()
        .ok_or("a file of the tmpfs has no object to map")?;
    let mut hash = FNV_BASIS;
    let mut piece = [0_u8; PIECE];
    let mut at = 0;
    while at < len {
        let byte = offset + at;
        let take = (len - at).min(PIECE as u64) as usize;
        let slot = piece.get_mut(..take).ok_or("a piece out of range")?;
        vmo.read_page(byte / PAGE_SIZE, (byte % PAGE_SIZE) as usize, slot)
            .map_err(|_| "the object of the file held across the release does not read")?;
        hash = fnv(hash, slot);
        at += take as u64;
    }
    Ok(hash)
}

/// An `Initrd` region of a made-up memory map, at 1 MiB for 1 MiB.
const REGION: MemRegion = MemRegion {
    base: 0x10_0000,
    len: 0x10_0000,
    kind: MemKind::Initrd,
    reserved: 0,
};

/// `mm::initrd_readable` and `mm::whole_frames`, the release's two pieces
/// of arithmetic, on made-up maps and spans: what keeps the archive's
/// reading inside its region and the direct map (`L.mm.100`), and its
/// give-back to the pages it fills whole (`L.mm.99`).
fn arithmetic() -> Result<(), &'static str> {
    let usable = MemRegion {
        kind: MemKind::Usable,
        base: 0x30_0000,
        ..REGION
    };
    let map = [usable, REGION];
    let wide = (0, 1 << 32);
    let readable = [
        // Unaligned at both ends, inside the region.
        ((0x10_0800, 0x5000), wide, true),
        // Across the region's end.
        ((0x1F_F000, 0x2000), wide, false),
        // In a region that is not `Initrd`, and in none at all.
        ((0x30_0000, 0x1000), wide, false),
        ((0x50_0000, 0x1000), wide, false),
        // Inside the region, past the direct map's end.
        ((0x10_0800, 0x5000), (0, 0x10_2000), false),
    ];
    if readable
        .iter()
        .any(|&(span, physmap, expected)| mm::initrd_readable(&map, span, physmap) != expected)
    {
        return Err("the archive's range test took or refused a range as it should not");
    }
    let array = 0..1 << 20;
    let whole = [
        // Unaligned at both ends: the pages it fills whole only.
        ((0x10_0800, 0x5000), array.clone(), 0x101..0x105),
        // Aligned.
        ((0x10_0000, 0x3000), array.clone(), 0x100..0x103),
        // The frame array ending inside it.
        ((0x10_0800, 0x5000), 0..0x103, 0x101..0x103),
        // Frame 0, which no allocator manages.
        ((0, 0x2000), array.clone(), 1..2),
        // No page filled whole.
        ((0x10_0800, 0x800), array, 0..0),
    ];
    if whole
        .into_iter()
        .any(|((base, len), array, expected)| mm::whole_frames(base, len, array) != expected)
    {
        return Err(
            "the archive's whole pages were counted other than inward and inside the array",
        );
    }
    Ok(())
}

/// How many holds a give-back of `frames` takes: one per aligned batch it
/// touches, and one for nothing.
fn holds_for(frames: &core::ops::Range<u64>) -> u64 {
    if frames.is_empty() {
        return 1;
    }
    (frames.end - 1) / mm::GIVE_BACK_BATCH - frames.start / mm::GIVE_BACK_BATCH + 1
}

/// Whether a name of the archive may still resolve after the release: a
/// spared one, `lib` above `/lib/firmware`, and a mount point or anything
/// beneath it, which resolves in the filesystem mounted there.
fn kept(name: &[u8]) -> bool {
    let first = name.split(|&byte| byte == b'/').next().unwrap_or_default();
    spared(name)
        || name == b"lib"
        || [&b"dev"[..], b"proc", b"sys", b"tmp", b"sysroot"].contains(&first)
}

/// Pid 1's inputs, one fingerprint over the three.
fn inputs_fingerprint() -> (u64, [(usize, usize); 3]) {
    let all = crate::init::input_bytes();
    let hash = all.iter().fold(FNV_BASIS, |hash, bytes| fnv(hash, bytes));
    (
        hash,
        all.map(|bytes| (bytes.as_ptr() as usize, bytes.len())),
    )
}

/// What the check saw before the release.
struct Before {
    /// The archive's physical base and length, if there was one.
    span: Option<(u64, u64)>,
    /// Its whole pages the allocator describes.
    frames: core::ops::Range<u64>,
    /// The frames managed.
    managed: u64,
    /// Pid 1's inputs' fingerprint.
    inputs: u64,
    /// Whether `/` was switched to a root disk, and if so the archive's
    /// names and a file held across the release.
    switched: Option<(Vec<Vec<u8>>, Held)>,
}

impl Before {
    fn take() -> Result<Before, &'static str> {
        let span = mm::initrd_span();
        let frames = span.map_or(0..0, |(base, len)| mm::initrd_frames(base, len));
        let switched = if root_disk::switched() {
            let names = mm::with_initrd(|archive| {
                let mut names = Vec::new();
                for name in initramfs::unpacked_names(archive) {
                    names
                        .try_reserve(1)
                        .map_err(|_| "no memory for the archive's names")?;
                    names.push(
                        crate::fallible::try_to_vec(name).map_err(|_| "no memory for a name")?,
                    );
                }
                Ok::<_, &'static str>(names)
            })
            .ok_or("the root was switched with no archive to compare its tmpfs with")??;
            let held = Held::first_of(&names)?;
            Some((names, held))
        } else {
            None
        };
        Ok(Before {
            span,
            frames,
            managed: mm::managed_frames(),
            inputs: inputs_fingerprint().0,
            switched,
        })
    }

    /// The release, as `released` says it went, against what was seen
    /// before it. Answers the line's words for the tmpfs.
    fn after(&self, released: &Released) -> Result<&'static str, &'static str> {
        if let Some((base, len)) = self.span {
            if !released.archive {
                return Err("the release gave back no archive though the boot had one");
            }
            let whole = self.frames.end - self.frames.start;
            if released.given.frames != whole {
                return Err("the release gave other than the archive's whole pages");
            }
            if released.given.largest_batch > mm::GIVE_BACK_BATCH
                || released.given.holds != holds_for(&self.frames)
            {
                return Err("the release gave more than one batch a hold, or in other holds");
            }
            if mm::managed_frames() != self.managed + whole {
                return Err("the managed frames did not grow by the archive's whole pages");
            }
            if mm::reserved_in(self.frames.clone()) != 0 {
                return Err("a whole page of the archive is still Reserved after its release");
            }
            if mm::watermark() != mm::watermark_for(mm::managed_frames())
                || released.given.watermark != mm::watermark()
            {
                return Err(
                    "the watermark is not watermark_for the frames managed after the release",
                );
            }
            if mm::with_initrd(|_| ()).is_some() || mm::initrd_span().is_some() {
                return Err("the archive still answers after its release");
            }
            let start = mm::direct_map(base) as usize;
            let end = start.saturating_add(len as usize);
            let (inputs, spans) = inputs_fingerprint();
            if spans
                .iter()
                .any(|&(at, len)| len != 0 && at < end && at.saturating_add(len) > start)
            {
                return Err("one of pid 1's inputs lies in the archive's released pages");
            }
            if inputs != self.inputs {
                return Err("pid 1's inputs changed across the release");
            }
        }
        let Some((names, held)) = &self.switched else {
            return if released.removed.is_some() {
                Err("the tmpfs was given back on a boot whose / is in memory")
            } else {
                Ok("the tmpfs, / in memory, kept")
            };
        };
        if released.removed.is_none() {
            return Err("the tmpfs was kept after a switch to a root disk");
        }
        let ns = namespace();
        let ctx = ns.context();
        if names
            .iter()
            .filter(|name| !kept(name))
            .any(|name| ns.resolve(&ctx, Some(&ctx.root), name, false).is_ok())
        {
            return Err("a name of the archive outside the spared set is still in the tmpfs");
        }
        if through_file(&held.program, held.len)? != held.through_file
            || through_object(&held.program, held.len)? != held.through_object
        {
            return Err("a file held open and mapped across the release reads otherwise");
        }
        Ok(
            "the tmpfs holds only the spared set, and a file held open and mapped across it reads as before",
        )
    }
}

/// The release, between the look before it and the look after; stops the
/// machine (FX-1203) on what is not as it should be.
///
/// Verifies: L.mm.71, L.mm.99, L.mm.100, L.init.5
pub fn run() -> Option<Released> {
    if let Err(problem) = arithmetic() {
        crate::panic::fatal!(
            catalog::RELEASE,
            "the release self-check failed before it: {problem}"
        );
    }
    let before = match Before::take() {
        Ok(before) => before,
        Err(problem) => crate::panic::fatal!(
            catalog::RELEASE,
            "the release self-check failed before it: {problem}"
        ),
    };
    let released = release()?;
    match before.after(&released) {
        Ok(tmpfs) => println!(
            "  release  checked: 5 maps and 5 spans counted as the archive's range test and whole \
             pages should; the managed frames up by the archive's {} whole pages in {} holds of at \
             most {} frames, none Reserved, the watermark from the new count, the archive answering nothing; pid 1's \
             inputs unchanged and outside it; {tmpfs}",
            released.given.frames,
            released.given.holds,
            mm::GIVE_BACK_BATCH,
        ),
        Err(problem) => {
            crate::panic::fatal!(catalog::RELEASE, "the release self-check failed: {problem}");
        }
    }
    Some(released)
}
