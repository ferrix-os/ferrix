//! The kernel's one remaining native loader (`docs/microkernel/K15.md` §2.4).
//!
//! Every process a program starts is mapped by its creator, with the library
//! `ferrix-process-builder`; the kernel parses no image a program can name in a
//! `personality=user` build. The ones it starts itself have no creator: `devmgr`,
//! and in that build the partition manager and `linuxd` (`docs/MICROKERNEL.md`
//! §5). This loads those, from bytes read out of the initramfs, which K14 vouches
//! for. In `present` and `desktop` `ferrix.devmgr=init` still has it parse what
//! pid 1 left at `/sbin/devmgr`, as those shapes always have (§2.4).
//!
//! Every page is anonymous and copied: nothing is mapped from a file, which keeps
//! a driver from faulting on a disk it serves (`docs/DEVMGR.md` §5). No page is
//! both writable and executable, and a malformed image is refused, never a stop.

use ferrix_bootinfo::PAGE_SIZE;
#[cfg(not(ferrix_restricted_window = "on"))]
use ferrix_bootinfo::USER_VIRT_END;

/// Below the window's root slot, which no image reaches (`restricted_window`, K11 §2.1).
#[cfg(ferrix_restricted_window = "on")]
const USER_VIRT_END: u64 = 255 << 39;
use ferrix_elf::{Class, Elf, ElfError, PF_R, PF_W, PF_X, PT_INTERP, Segment};
use ferrix_native_abi::types::{NATIVE_STACK_GUARD, NATIVE_STACK_SIZE};
use ferrix_vma::VmaFlags;

use crate::syscall::native::EntryPoint;
use crate::syscall::uaccess;
use crate::user::space::{AddressSpace, SpaceError};

/// Where a position-independent first image goes: the Linux loader's place, two
/// thirds of the way up the user half, which the library shares.
const PIE_BASE: u64 = (USER_VIRT_END / 3 * 2) & !(PAGE_SIZE - 1);

/// The top of its stack: as the library places it, below the last page.
const STACK_TOP: u64 = (USER_VIRT_END - PAGE_SIZE) & !15;

/// The most loadable segments a first image may have; a linker makes four.
const MAX_SEGMENTS: usize = 16;

/// Why an image was not loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The headers are malformed, or a segment is not inside the image.
    Malformed(ElfError),
    /// Another machine's or word width's.
    WrongMachine(u16),
    /// It names a dynamic linker.
    NeedsInterpreter,
    /// No loadable segment.
    Empty,
    /// Its entry or a segment is outside user space, at this address.
    NotUser(u64),
    /// This page would be writable and executable.
    WriteExecute(u64),
    /// The space refused.
    Space(SpaceError),
}

impl From<SpaceError> for Refused {
    fn from(error: SpaceError) -> Self {
        Refused::Space(error)
    }
}

/// A segment moved by the image's bias, every address in user space.
#[derive(Clone, Copy, Debug)]
struct Moved {
    first: u64,
    last: u64,
    flags: u32,
}

const fn page_down(address: u64) -> u64 {
    address & !(PAGE_SIZE - 1)
}

/// `segment`, moved, refused if any of it is outside user space.
fn moved(segment: &Segment, bias: u64) -> Result<Moved, Refused> {
    let at = segment.vaddr.wrapping_add(bias);
    let end = at
        .checked_add(segment.memsz)
        .filter(|&end| end <= USER_VIRT_END && at < USER_VIRT_END)
        .ok_or(Refused::NotUser(at))?;
    Ok(Moved {
        first: page_down(at),
        last: page_down(end.saturating_add(PAGE_SIZE - 1)),
        flags: segment.flags,
    })
}

/// The union of what the segments ask of the page at `page`.
fn flags_of<'a>(page: u64, segments: impl Iterator<Item = &'a Moved>) -> u32 {
    segments
        .filter(|segment| segment.first <= page && page < segment.last)
        .fold(0, |flags, segment| flags | segment.flags)
}

/// Read and judge `image`'s headers: its bias, entry and segments moved.
fn plan<'a>(
    image: &'a [u8],
    base: u64,
    segments: &mut [Option<Moved>; MAX_SEGMENTS],
) -> Result<(Elf<'a>, u64, u64), Refused> {
    let elf = Elf::parse(image).map_err(Refused::Malformed)?;
    let wide = matches!(elf.class(), Class::Elf64);
    if elf.machine() != crate::arch::ARCH.elf_machine() || wide != cfg!(target_pointer_width = "64")
    {
        return Err(Refused::WrongMachine(elf.machine()));
    }
    elf.validate_segments_within(image.len() as u64)
        .map_err(Refused::Malformed)?;
    if elf.segments().any(|segment| segment.kind == PT_INTERP) {
        return Err(Refused::NeedsInterpreter);
    }
    let (low, _) = elf.load_span(PAGE_SIZE).ok_or(Refused::Empty)?;
    let bias = if elf.is_pie() {
        base.wrapping_sub(low)
    } else {
        0
    };
    let entry = elf.entry().wrapping_add(bias);
    if entry >= USER_VIRT_END {
        return Err(Refused::NotUser(entry));
    }
    for (slot, segment) in segments.iter_mut().zip(elf.loadable()) {
        *slot = Some(moved(&segment, bias)?);
    }
    if elf.loadable().count() > MAX_SEGMENTS {
        return Err(Refused::Empty);
    }
    Ok((elf, bias, entry))
}

/// No page W+X: a page's answer can differ from its neighbour's only at a
/// segment's first or last page, so those are the ones judged.
fn no_page_writes_and_executes(segments: &[Option<Moved>]) -> Result<(), Refused> {
    for segment in segments.iter().flatten() {
        for at in [segment.first, segment.last.saturating_sub(PAGE_SIZE)] {
            let flags = flags_of(at, segments.iter().flatten());
            if flags & PF_W != 0 && flags & PF_X != 0 {
                return Err(Refused::WriteExecute(at));
            }
        }
    }
    Ok(())
}

/// Map every segment's pages writable, each page once. Headers ascend, as ELF
/// asks; ones that do not overlap a region and the space refuses them.
fn map_pages(space: &AddressSpace, segments: &[Option<Moved>]) -> Result<(), Refused> {
    let mut mapped_to = 0;
    for segment in segments.iter().flatten() {
        let from = segment.first.max(mapped_to);
        if from < segment.last {
            let _ = space.map_anonymous(from, segment.last - from, VmaFlags::READ_WRITE)?;
            mapped_to = segment.last;
        }
    }
    Ok(())
}

/// Give every page what its segments ask for, a run of pages that agree at a time.
fn apply_permissions(space: &AddressSpace, segments: &[Option<Moved>]) -> Result<(), Refused> {
    for segment in segments.iter().flatten() {
        let mut from = segment.first;
        while from < segment.last {
            let flags = flags_of(from, segments.iter().flatten());
            let mut to = from + PAGE_SIZE;
            while to < segment.last && flags_of(to, segments.iter().flatten()) == flags {
                to += PAGE_SIZE;
            }
            space.protect(from, to - from, permissions(flags))?;
            from = to;
        }
    }
    Ok(())
}

/// Load `image` into `space`, which must be empty: every loadable segment,
/// its file bytes copied, then permissions by page; the stack, with a guard
/// either side. Answers where it enters.
///
/// # Errors
///
/// [`Refused`]. The space may then hold part of the image; its process is ended
/// by the caller and never run.
pub(crate) fn load(space: &AddressSpace, image: &[u8]) -> Result<EntryPoint, Refused> {
    load_at(space, image, PIE_BASE)
}

/// [`load`], a position-independent image going to `base`: linuxd's, in its
/// region (`docs/microkernel/M5-LIFT.md` §6).
pub(crate) fn load_at(
    space: &AddressSpace,
    image: &[u8],
    base: u64,
) -> Result<EntryPoint, Refused> {
    let mut segments: [Option<Moved>; MAX_SEGMENTS] = [None; MAX_SEGMENTS];
    let (elf, bias, entry) = plan(image, base, &mut segments)?;
    no_page_writes_and_executes(&segments)?;
    map_pages(space, &segments)?;
    // The file bytes. `.bss` is already zero.
    for segment in elf.loadable() {
        let bytes = elf_bytes(image, &segment)?;
        uaccess::copy_to_user(space, segment.vaddr.wrapping_add(bias), bytes)
            .map_err(|_| Refused::Space(SpaceError::OutOfMemory))?;
    }
    apply_permissions(space, &segments)?;
    let stack = place_stack(space)?;
    Ok(EntryPoint { entry, stack })
}

/// `segment`'s file bytes: inside the image, as `validate_segments_within` said.
fn elf_bytes<'a>(image: &'a [u8], segment: &Segment) -> Result<&'a [u8], Refused> {
    let start = usize::try_from(segment.offset).ok();
    let len = usize::try_from(segment.filesz).ok();
    start
        .zip(len)
        .and_then(|(start, len)| image.get(start..start.checked_add(len)?))
        .ok_or(Refused::Malformed(ElfError::SegmentOutOfBounds))
}

/// The stack and its guards, where the library places a child's.
fn place_stack(space: &AddressSpace) -> Result<u64, Refused> {
    let low = STACK_TOP - NATIVE_STACK_SIZE;
    let _ = space.map_anonymous(low, NATIVE_STACK_SIZE, VmaFlags::READ_WRITE)?;
    let _ = space.map_anonymous(STACK_TOP, USER_VIRT_END - STACK_TOP, VmaFlags::NONE)?;
    let _ = space.map_anonymous(low - NATIVE_STACK_GUARD, NATIVE_STACK_GUARD, VmaFlags::NONE)?;
    Ok(STACK_TOP)
}

/// `PF_*` as region flags.
fn permissions(flags: u32) -> VmaFlags {
    VmaFlags {
        read: flags & PF_R != 0,
        write: flags & PF_W != 0,
        execute: flags & PF_X != 0,
        ..VmaFlags::NONE
    }
}
