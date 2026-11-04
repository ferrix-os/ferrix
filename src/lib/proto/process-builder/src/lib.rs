//! The native ELF loader, in the creator's process (`docs/microkernel/K15.md`).
//!
//! The kernel makes a process with an empty address space and enters it where
//! it is told. Everything between -- reading the image, choosing what goes
//! where -- is this crate, in whoever starts the process, so an image a program
//! chose is parsed in that program and never in ring 0. Zircon's process
//! builder is the reference.
//!
//! ```text
//! let image = image(vmo)?;                   // EXECUTE, no WRITE, no DUPLICATE
//! let built = build(&job, &image, "name", machine)?;  // made, mapped, space handle closed
//! built.process().notify_on_exit(&port, key)?;
//! let process = built.run(bootstrap)?;
//! ```
//!
//! # What goes where
//!
//! Every page of the image is one of two things, as in the Linux loader
//! (`src/kernel/src/syscall/load.rs`):
//!
//! * **Mapped from the image's VMO**, shared: a page of one non-writable
//!   segment's file contents, in no other segment, whose file offset and
//!   address agree within a page. The segment's first and last file pages are
//!   mapped whole. So text is never copied, sibling processes share it, and
//!   a measured job's rule (K14: "executable mappings come only from sealed VMOs")
//!   needs only [`sealed_image`] in place of [`image`], which stays unsealed.
//! * **Private**: allocated writable, the file bytes written through
//!   `space_write`, then protected to what its segments ask for. The union of a
//!   page's segments' permissions.
//!
//! **An executable page is never private** (ledger 5726 (2)): one that cannot be
//! mapped refuses the image, and so does a page writable and executable.
//!
//! # Who pays
//!
//! Private pages, tables and the stack: the job the process was in when
//! `process_space` answered, by K2's owner slot; a job at its limit fails the
//! build with `NO_MEMORY`. Mapped pages are the VMO's frames, charged to its
//! maker: `docs/certification/MEMORY-AND-TIMING.md`.
//!
//! # No allocator, no unsafe
//!
//! Rust's `core` only, so every native program can link it, and a recorder plays
//! the kernel in the tests, as for `ferrix-native`.

#![no_std]
#![forbid(unsafe_code)]

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};
use ferrix_elf::{Class, Elf, ElfError, PF_R, PF_W, PF_X, PT_INTERP};
use ferrix_native::Error;
use ferrix_native::handle::OwnedHandle;
use ferrix_native::job::Job;
use ferrix_native::pending::{Process, make_process};
use ferrix_native::space::{Access, Space};
use ferrix_native::vmo::Vmo;
use ferrix_native::{Requested, Rights, Syscall};
use ferrix_native_abi::nr::PROCESS_NAME_MAX;
use ferrix_native_abi::rights::IMAGE_RIGHTS;
use ferrix_native_abi::types::{NATIVE_STACK_GUARD, NATIVE_STACK_SIZE};

#[cfg(test)]
mod tests;

/// The largest image [`build`] takes, in bytes: `process_create`'s bound, so a
/// starter that moved sees the same answer.
pub const MAX_IMAGE_BYTES: u64 = 16 << 20;

/// The most `PT_LOAD` segments an image may have. A linker makes four.
pub const MAX_SEGMENTS: usize = 16;

/// The most pieces one image's pages divide into, cut at each segment's four
/// page boundaries.
const MAX_CUTS: usize = MAX_SEGMENTS * 4;

/// The bytes read at once: a page, on the starter's stack.
const CHUNK: usize = 4096;

/// What the rights of a space handle the loader holds are: it maps and writes,
/// never reads, and is closed before the process runs.
const BUILD_RIGHTS: Rights = Rights(Rights::MAP.0 | Rights::EXECUTE.0 | Rights::WRITE.0);

/// Where a static PIE's lowest page goes: the kernel loader's choice, two thirds
/// of the way up the user half.
const PIE_BASE: u64 = (USER_VIRT_END / 3 * 2) & !(PAGE_SIZE - 1);

/// The top of the stack, as the Linux loader places it, below the last page.
const STACK_TOP: u64 = (USER_VIRT_END - PAGE_SIZE) & !15;

/// Why an image was not built into a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// A call was refused. For `NO_MEMORY` the new process's job is at its limit.
    Call(Error),
    /// The headers are malformed.
    Malformed(ElfError),
    /// Larger than [`MAX_IMAGE_BYTES`].
    TooBig,
    /// More than [`MAX_SEGMENTS`] loadable segments, or none.
    Segments,
    /// Another machine's or word width's.
    WrongMachine(u16),
    /// It names a dynamic linker.
    NeedsInterpreter,
    /// Its entry point or a segment is outside user space.
    NotUser(u64),
    /// A page both writable and executable.
    WriteExecute(u64),
    /// An executable page that cannot be mapped from the image.
    Unmappable(u64),
    /// A segment with no permission at all.
    NoAccess(u64),
    /// A name longer than `nr::PROCESS_NAME_MAX`.
    Name,
}

impl From<Error> for Refused {
    fn from(error: Error) -> Self {
        Refused::Call(error)
    }
}

/// An image a process can be built from, made of `vmo`, consuming the handle:
/// `vmo_executable`, then narrowed to [`IMAGE_RIGHTS`], so that it can be neither
/// written through nor duplicated. An earlier duplicate that kept `WRITE` still
/// reaches the frames, and a maker holding one can change a running child's text
/// (`VULNERABILITY-ANALYSIS.md`); [`sealed_image`] closes that, for a measured job.
///
/// # Errors
///
/// The status of either call. The handle is closed either way.
pub fn image<S: Syscall>(vmo: Vmo<S>) -> Result<Vmo<S>, Error> {
    let vmo = vmo.executable().map_err(|(error, _)| error)?;
    vmo.replace(Requested::Exactly(IMAGE_RIGHTS))
        .map_err(|(error, _)| error)
}

/// [`image`], sealed: what a process in a measured job can be built from
/// (`docs/microkernel/K14.md` §2). The frames are frozen for every holder, so an earlier
/// duplicate no longer reaches them, and the kernel records the digest as the image is mapped.
///
/// # Errors
///
/// The status of either call. The handle is closed either way.
pub fn sealed_image<S: Syscall>(vmo: Vmo<S>) -> Result<Vmo<S>, Error> {
    let vmo = vmo.sealed_executable().map_err(|(error, _)| error)?;
    vmo.replace(Requested::Exactly(IMAGE_RIGHTS))
        .map_err(|(error, _)| error)
}

/// A process with its image mapped, not started.
#[derive(Debug)]
pub struct Built<S: Syscall> {
    process: Process<S>,
    entry: u64,
    stack: u64,
}

impl<S: Syscall> Built<S> {
    /// The process, for what is done before it runs: a watch on its end.
    #[must_use]
    pub const fn process(&self) -> &Process<S> {
        &self.process
    }

    /// Where it will enter.
    #[must_use]
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// The stack it will start on.
    #[must_use]
    pub const fn stack(&self) -> u64 {
        self.stack
    }

    /// `process_run`: start it, giving it `bootstrap`, and answer the process.
    ///
    /// # Errors
    ///
    /// The status, the process (killed when it is dropped) and the handle back.
    pub fn run(self, bootstrap: OwnedHandle<S>) -> Result<Process<S>, (Error, OwnedHandle<S>)> {
        let (Ok(entry), Ok(stack)) = (usize::try_from(self.entry), usize::try_from(self.stack))
        else {
            return Err((Error::InvalidArgs, bootstrap));
        };
        self.process.run(entry, stack, bootstrap)?;
        Ok(self.process)
    }
}

/// One loadable segment, moved by the image's bias.
#[derive(Debug, Clone, Copy, Default)]
struct Moved {
    at: u64,
    offset: u64,
    filesz: u64,
    memsz: u64,
    flags: u32,
}

impl Moved {
    /// One past the last byte of its file contents.
    const fn file_end(&self) -> u64 {
        self.at.wrapping_add(self.filesz)
    }

    /// One past its last byte.
    const fn mem_end(&self) -> u64 {
        self.at.wrapping_add(self.memsz)
    }

    /// Its whole pages.
    fn pages(&self) -> (u64, u64) {
        (page_down(self.at), page_up(self.mem_end()))
    }

    /// The pages of it that can be the file's, if it may be mapped at all: from
    /// its first page to its last file page, less a last page its `.bss` shares.
    fn mapped_pages(&self) -> Option<(u64, u64)> {
        if self.flags & PF_W != 0
            || self.filesz == 0
            || !self.at.wrapping_sub(self.offset).is_multiple_of(PAGE_SIZE)
            || page_down(self.at) > self.at
        {
            return None;
        }
        let end = if self.mem_end() <= self.file_end() {
            page_up(self.file_end())
        } else {
            page_down(self.file_end())
        };
        Some((page_down(self.at), end))
    }

    /// The VMO offset the page at `page` of this segment is at, whole.
    fn offset_of(&self, page: u64) -> Option<u64> {
        self.offset.checked_add(page)?.checked_sub(self.at)
    }
}

const fn page_down(address: u64) -> u64 {
    address & !(PAGE_SIZE - 1)
}

const fn page_up(address: u64) -> u64 {
    match address.checked_add(PAGE_SIZE - 1) {
        Some(up) => page_down(up),
        None => page_down(u64::MAX),
    }
}

/// `value` as a register, which on ARMv7-A is narrower than the ELF's numbers.
fn word(value: u64) -> Result<usize, Refused> {
    usize::try_from(value).map_err(|_| Refused::NotUser(value))
}

/// Read and check the image's headers: its segments, moved, and its entry point.
///
/// # Errors
///
/// [`Refused`] for everything wrong with the image that the kernel's loader
/// refuses, before anything is made.
fn plan<S: Syscall>(
    image: &Vmo<S>,
    machine: u16,
    segments: &mut [Moved; MAX_SEGMENTS],
) -> Result<(usize, u64), Refused> {
    let size = image.size()?;
    if size > MAX_IMAGE_BYTES {
        return Err(Refused::TooBig);
    }
    let mut head = [0_u8; CHUNK];
    let read = usize::try_from(size).unwrap_or(CHUNK).min(CHUNK);
    let head = head.get_mut(..read).ok_or(Refused::TooBig)?;
    image.read(head, 0)?;
    let elf = Elf::parse(head).map_err(Refused::Malformed)?;
    let wide = matches!(elf.class(), Class::Elf64);
    if elf.machine() != machine || wide != cfg!(target_pointer_width = "64") {
        return Err(Refused::WrongMachine(elf.machine()));
    }
    elf.validate_segments_within(size)
        .map_err(Refused::Malformed)?;
    if elf.segments().any(|segment| segment.kind == PT_INTERP) {
        return Err(Refused::NeedsInterpreter);
    }
    let (low, _) = elf.load_span(PAGE_SIZE).ok_or(Refused::Segments)?;
    let bias = if elf.is_pie() {
        PIE_BASE.wrapping_sub(low)
    } else {
        0
    };
    let mut count = 0;
    for segment in elf.loadable() {
        let moved = Moved {
            at: segment.vaddr.wrapping_add(bias),
            offset: segment.offset,
            filesz: segment.filesz,
            memsz: segment.memsz,
            flags: segment.flags,
        };
        if moved.flags & (PF_R | PF_W | PF_X) == 0 {
            return Err(Refused::NoAccess(moved.at));
        }
        if moved.at >= USER_VIRT_END
            || moved.mem_end() > USER_VIRT_END
            || moved.mem_end() < moved.at
        {
            return Err(Refused::NotUser(moved.at));
        }
        let slot = segments.get_mut(count).ok_or(Refused::Segments)?;
        *slot = moved;
        count += 1;
    }
    let entry = elf.entry().wrapping_add(bias);
    if entry >= USER_VIRT_END {
        return Err(Refused::NotUser(entry));
    }
    Ok((count, entry))
}

/// Where the answer for a page can change: each segment's four page boundaries,
/// sorted, each once.
fn cuts_of(segments: &[Moved], cuts: &mut [u64; MAX_CUTS]) -> usize {
    let mut count = 0;
    for segment in segments {
        let (first, last) = segment.pages();
        for at in [
            first,
            last,
            page_down(segment.file_end()),
            page_up(segment.file_end()),
        ] {
            if let Some(slot) = cuts.get_mut(count) {
                *slot = at;
                count += 1;
            }
        }
    }
    let used = cuts.get_mut(..count).unwrap_or_default();
    used.sort_unstable();
    let mut kept = 0;
    for index in 0..count {
        let at = cuts.get(index).copied().unwrap_or(0);
        if kept == 0 || cuts.get(kept - 1).copied() != Some(at) {
            if let Some(slot) = cuts.get_mut(kept) {
                *slot = at;
            }
            kept += 1;
        }
    }
    kept
}

/// What a span of pages, in which nothing changes, is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    /// In no segment: a hole, mapped with no access, as the kernel's loader
    /// holds a hole in the span.
    Hole,
    /// Mapped from the VMO at this offset, with this access.
    Mapped(u64, Access),
    /// Private, with the union of its segments' permissions.
    Private(Access),
}

/// Judge `[from, to)`.
fn judge(segments: &[Moved], from: u64, to: u64) -> Result<Piece, Refused> {
    let mut flags = 0;
    let mut covering = 0;
    let mut only = None;
    for segment in segments {
        let (first, last) = segment.pages();
        if first < to && from < last {
            flags |= segment.flags;
            covering += 1;
            only = Some(segment);
        }
    }
    let Some(only) = only else {
        return Ok(Piece::Hole);
    };
    if flags & PF_W != 0 && flags & PF_X != 0 {
        return Err(Refused::WriteExecute(from));
    }
    let access = if flags & PF_X != 0 {
        Access::ReadExecute
    } else if flags & PF_W != 0 {
        Access::ReadWrite
    } else {
        Access::Read
    };
    let mapped = (covering == 1)
        .then(|| only.mapped_pages())
        .flatten()
        .filter(|&(first, end)| first <= from && to <= end)
        .and_then(|_| only.offset_of(from));
    match mapped {
        Some(offset) => Ok(Piece::Mapped(offset, access)),
        None if flags & PF_X != 0 => Err(Refused::Unmappable(from)),
        None => Ok(Piece::Private(access)),
    }
}

/// Copy the file bytes of `segments` that fall in `[from, to)` into the space.
fn copy_into<S: Syscall>(
    image: &Vmo<S>,
    space: &Space<S>,
    segments: &[Moved],
    from: u64,
    to: u64,
) -> Result<(), Refused> {
    let mut chunk = [0_u8; CHUNK];
    for segment in segments {
        let start = segment.at.max(from);
        let end = segment.file_end().min(to);
        let mut at = start;
        while at < end {
            let take = usize::try_from(end - at).unwrap_or(CHUNK).min(CHUNK);
            let buffer = chunk.get_mut(..take).ok_or(Refused::TooBig)?;
            let offset = segment
                .offset_of(at)
                .ok_or(Refused::Malformed(ElfError::SegmentOutOfBounds))?;
            image.read(buffer, offset)?;
            let wrote = space.write(word(at)?, buffer)?;
            if wrote != take {
                return Err(Refused::Call(Error::NoMemory));
            }
            at += take as u64;
        }
    }
    Ok(())
}

/// Map or copy one piece.
fn place<S: Syscall>(
    image: &Vmo<S>,
    space: &Space<S>,
    segments: &[Moved],
    (from, to): (u64, u64),
    piece: Piece,
) -> Result<(), Refused> {
    let (at, length) = (word(from)?, word(to - from)?);
    match piece {
        Piece::Hole => {
            let _ = space.allocate(Some(at), length, Access::None)?;
        }
        Piece::Mapped(offset, access) => {
            let _ = space.map_no_pager(image, Some(at), length, access, offset)?;
        }
        Piece::Private(access) => {
            let _ = space.allocate(Some(at), length, Access::ReadWrite)?;
            copy_into(image, space, segments, from, to)?;
            if access != Access::ReadWrite {
                space.protect(at, length, access)?;
            }
        }
    }
    Ok(())
}

/// Place every piece, in order, a mapped piece that carries on from the last
/// one being one region with it, so a segment is one mapping.
fn place_all<S: Syscall>(
    image: &Vmo<S>,
    space: &Space<S>,
    segments: &[Moved],
    cuts: &[u64],
) -> Result<(), Refused> {
    let mut pending: Option<((u64, u64), Piece)> = None;
    for pair in cuts.windows(2) {
        let [from, to] = *pair else { continue };
        let piece = judge(segments, from, to)?;
        if let Some(((start, end), Piece::Mapped(offset, access))) = pending
            && let Piece::Mapped(next, same) = piece
            && end == from
            && same == access
            && next == offset.wrapping_add(end - start)
        {
            pending = Some(((start, to), Piece::Mapped(offset, access)));
            continue;
        }
        if let Some((span, last)) = pending.replace(((from, to), piece)) {
            place(image, space, segments, span, last)?;
        }
    }
    if let Some((span, last)) = pending {
        place(image, space, segments, span, last)?;
    }
    Ok(())
}

/// The stack, and a guard either side of it.
fn place_stack<S: Syscall>(space: &Space<S>) -> Result<u64, Refused> {
    let low = STACK_TOP - NATIVE_STACK_SIZE;
    let _ = space.allocate(
        Some(word(low)?),
        word(NATIVE_STACK_SIZE)?,
        Access::ReadWrite,
    )?;
    let _ = space.allocate(
        Some(word(STACK_TOP)?),
        word(USER_VIRT_END - STACK_TOP)?,
        Access::None,
    )?;
    let _ = space.allocate(
        Some(word(low - NATIVE_STACK_GUARD)?),
        word(NATIVE_STACK_GUARD)?,
        Access::None,
    )?;
    Ok(STACK_TOP)
}

/// The ELF machine of the architecture `std::env::consts::ARCH` names, for a Linux
/// program that starts native ones; a native one has `ferrix_rt::ELF_MACHINE`. Asked
/// of the caller since no library holds a `target_arch` conditional.
#[must_use]
pub fn machine_named(arch: &str) -> u16 {
    match arch {
        "x86_64" => ferrix_elf::EM_X86_64,
        "aarch64" => ferrix_elf::EM_AARCH64,
        "arm" => ferrix_elf::EM_ARM,
        _ => 0,
    }
}

/// A process in `job`, named `name`, with `image` of ELF machine `machine` mapped and its stack ready, not
/// started. The creator holds no handle to its space when this returns.
///
/// # Errors
///
/// [`Refused`]. On a failure after the process was made, its handle is dropped,
/// which ends an unstarted process.
pub fn build<S: Syscall>(
    job: &Job<S>,
    image: &Vmo<S>,
    name: &str,
    machine: u16,
) -> Result<Built<S>, Refused> {
    if name.len() > PROCESS_NAME_MAX {
        return Err(Refused::Name);
    }
    let mut store = [Moved::default(); MAX_SEGMENTS];
    let (count, entry) = plan(image, machine, &mut store)?;
    let segments = store.get(..count).ok_or(Refused::Segments)?;
    let mut cuts = [0_u64; MAX_CUTS];
    let cut_count = cuts_of(segments, &mut cuts);
    let cuts = cuts.get(..cut_count).ok_or(Refused::Segments)?;
    // Every piece judged before the process is made: an image refused for what
    // it is costs its job nothing.
    for pair in cuts.windows(2) {
        if let [from, to] = *pair {
            let _ = judge(segments, from, to)?;
        }
    }
    let process = make_process(job, name)?;
    let space = process.space(Requested::Exactly(BUILD_RIGHTS))?;
    place_all(image, &space, segments, cuts)?;
    let stack = place_stack(&space)?;
    drop(space);
    Ok(Built {
        process,
        entry,
        stack,
    })
}
