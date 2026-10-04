//! Turning an ELF image into an address space a program can run in.
//!
//! `src/lib/platform/elf` already parses, validates and is fuzzed; what it deliberately
//! does not do is touch memory. This is the half that does: it takes a parsed
//! image and a fresh [`AddressSpace`] and leaves behind the mappings, the
//! entry point, and the three numbers the auxiliary vector needs to tell the
//! program where its own program headers are.
//!
//! # Mapped from the file where a page is the file's, copied where it is not
//!
//! A program read from a file whose pages are an object -- tmpfs, and btrfs
//! through its page cache -- is not copied. Every page wholly inside one
//! segment's file contents, and in no other segment, is mapped from the
//! file's object, privately: read, and executed, where it is in the file,
//! read from the disk only when the program first touches it, and copied into
//! the mapping's own object the first time the program writes it, so a
//! writable segment's writes never reach the file. That is what lets a
//! program of any size start at the cost of its headers, which Chrome, at
//! 198 MB, needs, and what lets two processes running one program share its
//! text.
//!
//! The rest of the image is anonymous memory, and the loader copies into it:
//! the partial pages at a segment's two ends, which hold bytes of no segment
//! or of `.bss`; a page two segments share; every page of a segment whose
//! file offset and address disagree within a page, which no mapping can
//! express; and all of an image that came from no file, or from a file with no
//! object to map, which is copied through a small buffer. So a program from
//! anywhere loads, and only the one kind is paged in on demand.
//!
//! # Map first, copy second
//!
//! The pages arrive on fault, so a region has to exist before anything can be
//! written into it. That ordering also gives `.bss` for free: a committed
//! anonymous page is already zeroed, so the excess of `p_memsz` over
//! `p_filesz` needs nothing done to it. Copying zeroes over it would be
//! slower and would commit pages the program may never touch.
//!
//! # Why the permissions are computed per page and not per segment
//!
//! Because segments do not have to start on page boundaries, and two of them
//! can share one. Mapping each segment separately would then either overlap —
//! which the region map refuses, correctly — or silently give the shared page
//! one segment's permissions and not the other's.
//!
//! So the anonymous part is mapped writable, everything is copied in, and then
//! the permissions are applied over runs of pages that agree. A page covered
//! by two segments gets the union of their permissions, which is the only
//! answer that lets both segments work. A page mapped from the file belongs
//! to one segment and takes its permissions as it is mapped.
//!
//! # Two images, when the program names a linker
//!
//! A dynamically linked program is not loaded and entered. Its `PT_INTERP`
//! names a dynamic linker, and what the kernel does is load *both* images —
//! the program at its own base, the linker at [`INTERP_BASE`] — and enter the
//! linker, telling it through the auxiliary vector where the program is
//! (`AT_PHDR`, `AT_PHNUM`, `AT_ENTRY`) and where the linker itself landed
//! (`AT_BASE`). Resolving symbols and jumping to `AT_ENTRY` is then the
//! linker's job and none of the kernel's. That division is Linux's, and it is
//! why the kernel half of dynamic linking is small and the other half is not.
//!
//! The heap still starts past the *program*, not past the linker: it is the
//! program's `brk`, and the linker is placed far enough below that the two
//! cannot meet.
//!
//! And if that union comes out writable *and* executable, the image is
//! refused. Ferrix sweeps its own mappings for W^X at boot and would be
//! building one here on purpose otherwise. It does not happen for a binary
//! linked in this decade — `ld -z separate-code` has been the default for
//! years, precisely so that text and data never share a page — so refusing
//! costs nothing and names the problem if it ever appears.
//!
//! # A static PIE is placed, not relocated
//!
//! rustc links a musl program as a static position-independent executable
//! unless told otherwise: `ET_DYN`, no interpreter, and a self-relocating
//! start (musl's `rcrt1.o`). Linux maps such an image at a base of its own
//! choosing and applies no relocation; the program finds its base from
//! `AT_PHDR` less the program headers' link address and relocates itself before
//! anything reads an absolute address. So the loader does the same: every
//! address the image names is moved by [`PIE_BASE`] less the image's lowest
//! page, the entry, `AT_PHDR` and the heap's start with it, and `AT_BASE`, the
//! interpreter's base, stays zero because there is none.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END, is_user_address};
use ferrix_elf::{Elf, ElfError, PF_R, PF_W, PF_X, PT_INTERP, Segment};
use ferrix_linux_abi::errno::Errno;
use ferrix_vma::VmaFlags;

use crate::arch;
use crate::syscall::program::{self, ProgramFile};
use crate::syscall::uaccess::{self, UserError};
use crate::trap::Abi;
use crate::user::space::{AddressSpace, FileMapping, FilePlace, SpaceError};

/// Where an image's bytes are.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source<'a> {
    /// All of them, in kernel memory: a program built into the kernel, or one
    /// a check assembled. Copied in.
    Bytes(&'a [u8]),
    /// A file, of which only the headers have been read. Mapped where it can
    /// be, and copied a piece at a time where it cannot.
    File(&'a ProgramFile),
}

impl<'a> Source<'a> {
    /// The image's first bytes, which hold its file header and program
    /// headers: all of it, for bytes in memory.
    pub(crate) fn head(self) -> &'a [u8] {
        match self {
            Source::Bytes(bytes) => bytes,
            Source::File(file) => file.head(),
        }
    }

    /// The length of the whole image.
    fn len(self) -> u64 {
        match self {
            Source::Bytes(bytes) => bytes.len() as u64,
            Source::File(file) => file.len(),
        }
    }
}

/// The longest `PT_INTERP` read from a file when the headers did not hold it:
/// Linux's `PATH_MAX`.
const INTERPRETER_MOST: u64 = 4096;

/// Where a static PIE's lowest page is placed: two thirds of the way up the
/// user half, page-aligned, as Linux's `ELF_ET_DYN_BASE` puts an `ET_DYN` image
/// on every architecture. Far above where a fixed-address program is linked,
/// and far below the stack and the mappings that grow down from it.
pub(crate) const PIE_BASE: u64 = pie_base(Abi::Native);

/// Where a dynamic linker's lowest page is placed: one third of the way up the
/// user half, page-aligned.
///
/// Below [`PIE_BASE`] and not above it, because what grows is the program's
/// heap, which starts where the program ends. A linker placed above the
/// program would be a wall the heap runs into; placed below, the whole span
/// from the program up to the stack stays the program's. It is a fixed address
/// rather than one `mmap` chooses because nothing else here allocates before
/// the image is placed, and a fixed one is reproducible in a check.
pub(crate) const INTERP_BASE: u64 = interp_base(Abi::Native);

const _: () = assert!(
    INTERP_BASE < PIE_BASE && interp_base(Abi::Compat) < pie_base(Abi::Compat),
    "the linker must be placed below the program, not in its heap's way"
);

/// One past the highest address a program running in `abi` may use: the user
/// half for the machine's own programs, and 4 GiB less a page for a 32-bit
/// program on a 64-bit kernel (`docs/I386.md` §3.4).
pub(crate) const fn user_end(abi: Abi) -> u64 {
    match abi {
        Abi::Native => USER_VIRT_END,
        Abi::Compat => arch::COMPAT_USER_END,
    }
}

/// [`PIE_BASE`] for a program running in `abi`: the same two thirds, of the
/// space that program has.
pub(crate) const fn pie_base(abi: Abi) -> u64 {
    (user_end(abi) / 3 * 2) & !(PAGE_SIZE - 1)
}

/// [`INTERP_BASE`] for a program running in `abi`: the same third, of the
/// space that program has.
pub(crate) const fn interp_base(abi: Abi) -> u64 {
    (user_end(abi) / 3) & !(PAGE_SIZE - 1)
}

/// What the loader learned, and the program needs to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Loaded {
    /// `AT_ENTRY`: the *program's* entry point, wherever it was placed. Not
    /// where the processor starts when there is a dynamic linker -- that is
    /// [`Loaded::start`] -- because the linker is what jumps here, once it has
    /// resolved what the program imports.
    pub(crate) entry: u64,
    /// Where the processor starts: the linker's entry when the program named
    /// one, and the program's own when it did not.
    pub(crate) start: u64,
    /// `AT_PHDR`: where the program headers ended up in memory. Zero if no
    /// loadable segment covers them, which a static binary's do.
    pub(crate) phdr: u64,
    /// `AT_PHENT`: the size of one program header.
    pub(crate) phent: u64,
    /// `AT_PHNUM`: how many there are.
    pub(crate) phnum: u64,
    /// One past the highest address the image occupies, which is where the
    /// heap goes.
    pub(crate) end: u64,
    /// `AT_BASE`: where the dynamic linker was placed, or zero when the
    /// program named none and so is its own linker.
    pub(crate) base: u64,
    /// The ABI the program runs in, which its image's class and machine say:
    /// what mode it is entered in, how wide its startup stack's words are,
    /// and how much of the address space it may have.
    pub(crate) abi: Abi,
}

/// Why an image could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadError {
    /// `src/lib/platform/elf` refused it.
    Malformed(ElfError),
    /// Built for another architecture, or, for a dynamic linker, for another
    /// ABI than the program it would link.
    WrongMachine(u16),
    /// The image names an interpreter and the caller supplied none: a
    /// dynamically linked program handed to a loader that only places one
    /// image.
    NeedsInterpreter,
    /// The image names an interpreter and does not say which: a `PT_INTERP`
    /// that is not a path.
    BadInterpreter,
    /// The interpreter is not an `ET_DYN`. A linker is placed wherever the
    /// kernel puts it and relocates itself; one linked to a fixed address
    /// could not be placed at all.
    InterpreterNotPie,
    /// The interpreter names an interpreter of its own. Linux refuses this
    /// rather than following the chain, and so does this.
    InterpreterChain,
    /// It has no loadable segments at all.
    Empty,
    /// A page would have to be both writable and executable.
    WriteExecute(u64),
    /// The entry point is not a user address.
    EntryNotUser(u64),
    /// The address space refused a mapping.
    Space(SpaceError),
    /// A segment's contents could not be written into the space.
    Copy(UserError),
    /// A segment's contents, or the interpreter's name, could not be read
    /// from the file.
    Read(Errno),
}

impl From<SpaceError> for LoadError {
    fn from(error: SpaceError) -> Self {
        LoadError::Space(error)
    }
}

impl From<UserError> for LoadError {
    fn from(error: UserError) -> Self {
        LoadError::Copy(error)
    }
}

/// Refuse what [`load`] would refuse about the image itself, without touching
/// any memory.
///
/// For `execve`, which has to decide whether the call can still fail before it
/// takes the running program's memory away. Everything checked here is a
/// property of the bytes; what is left for [`load`] to find -- a page that
/// would be writable and executable, a mapping the space refuses -- is found
/// after the point of no return, and kills the process as it does on Linux.
///
/// Everything checked here is read from the headers alone: for a program in a
/// file, nothing past them has been read yet.
///
/// # Errors
///
/// The [`LoadError`] [`load`] would return for the same image.
pub(crate) fn check(image: Source<'_>, interpreter: Option<Source<'_>>) -> Result<(), LoadError> {
    let (elf, abi) = parse(image)?;
    let (low, _) = elf.load_span(PAGE_SIZE).ok_or(LoadError::Empty)?;
    let bias = bias_of(&elf, low, interpreter.is_some(), abi)?;
    let _ = entry_point(&elf, bias, abi)?;
    if let Some(source) = interpreter {
        let (linker, _) = parse_linker(source, abi)?;
        let (low, _) = linker.load_span(PAGE_SIZE).ok_or(LoadError::Empty)?;
        let _ = entry_point(&linker, interp_base(abi).wrapping_sub(low), abi)?;
    }
    Ok(())
}

/// Parse the image's headers and refuse what is wrong with them: a machine,
/// or a class, this kernel cannot run, and segments whose contents are not
/// inside the image, which for a file is inside the file rather than inside
/// the bytes read of it. Answers the ABI the image runs in.
fn parse(image: Source<'_>) -> Result<(Elf<'_>, Abi), LoadError> {
    let elf = Elf::parse(image.head()).map_err(LoadError::Malformed)?;
    let abi = arch::image_abi(elf.class(), elf.machine())
        .ok_or(LoadError::WrongMachine(elf.machine()))?;
    elf.validate_segments_within(image.len())
        .map_err(LoadError::Malformed)?;
    Ok((elf, abi))
}

/// Parse a dynamic linker for a program running in `abi`, and refuse one
/// that could not be its linker: another ABI's -- a 64-bit `ld.so` cannot
/// link an i386 program, whose `PT_INTERP` names its own -- one that must be
/// placed at a fixed address, or one that asks for a linker itself.
fn parse_linker(source: Source<'_>, abi: Abi) -> Result<(Elf<'_>, Abi), LoadError> {
    let (linker, linker_abi) = parse(source)?;
    if linker_abi != abi {
        return Err(LoadError::WrongMachine(linker.machine()));
    }
    if !linker.is_pie() {
        return Err(LoadError::InterpreterNotPie);
    }
    if linker.interpreter().is_some() {
        return Err(LoadError::InterpreterChain);
    }
    Ok((linker, linker_abi))
}

/// The path of the linker `image` asks for, if it asks for one.
///
/// For `execve`, which has to open that file before it can load anything;
/// the loader is handed the file, not the name.
///
/// The name is almost always in the first page, which the headers were read
/// with; a file whose `PT_INTERP` is further in has it read from there.
///
/// # Errors
///
/// [`LoadError::BadInterpreter`] for an image that asks for a linker and does
/// not say which, [`LoadError::Read`] for a name that could not be read, and
/// whatever `src/lib/platform/elf` refuses about the image.
pub(crate) fn interpreter_of(image: Source<'_>) -> Result<Option<Vec<u8>>, LoadError> {
    let elf = Elf::parse(image.head()).map_err(LoadError::Malformed)?;
    let Some(segment) = elf.segments().find(|segment| segment.kind == PT_INTERP) else {
        return Ok(None);
    };
    let data = match (segment.data(image.head()), image) {
        (Ok(data), _) => data.to_vec(),
        (Err(_), Source::File(file)) if segment.filesz <= INTERPRETER_MOST => {
            let mut bytes = vec![0_u8; usize::try_from(segment.filesz).unwrap_or(0)];
            file.read_exact_at(segment.offset, &mut bytes)
                .map_err(LoadError::Read)?;
            bytes
        }
        (Err(_), _) => return Err(LoadError::BadInterpreter),
    };
    let path = ferrix_elf::interpreter_name(&data).map_err(|_| LoadError::BadInterpreter)?;
    Ok(Some(path.to_vec()))
}

/// How far the image is moved from where it is linked: zero for a
/// fixed-address image, and from its lowest page `low` to [`pie_base`] of the
/// ABI it runs in for one that may be placed.
///
/// `linked` says whether the caller brought a dynamic linker. An image that
/// names one and was given none is refused whatever its type: an `ET_EXEC`
/// naming a `PT_INTERP` is as unrunnable without its linker as an `ET_DYN` is,
/// and placing it and entering it would run a program whose every imported
/// symbol is an unrelocated zero.
fn bias_of(elf: &Elf<'_>, low: u64, linked: bool, abi: Abi) -> Result<u64, LoadError> {
    if !linked && elf.segments().any(|segment| segment.kind == PT_INTERP) {
        return Err(LoadError::NeedsInterpreter);
    }
    if !elf.is_pie() {
        return Ok(0);
    }
    Ok(pie_base(abi).wrapping_sub(low))
}

/// Where execution starts, if a program may run there.
///
/// An entry point outside the user half is refused as a property of the image,
/// before anything is mapped, because entering it does not merely fault in the
/// program. x86-64 enters user mode with `sysretq`, which takes the address in
/// `RCX` and raises `#GP` *in ring 0* when that address is not canonical, so
/// the kernel would take the program's mistake as its own fault.
///
/// On ARMv7-A bit 0 of the entry point says Thumb, and the instruction is at
/// the address with that bit clear. The test needs no mask for it: the bound is
/// even, so clearing bit 0 cannot move an address from one side of it to the
/// other, and the answer is the same with the bit or without it.
///
/// A 32-bit program's entry has to be inside its own, smaller space too:
/// compatibility mode cannot reach past 4 GiB.
fn entry_point(elf: &Elf<'_>, bias: u64, abi: Abi) -> Result<u64, LoadError> {
    const _: () = assert!(
        USER_VIRT_END.is_multiple_of(2),
        "the Thumb bit could cross the bound"
    );
    let entry = elf.entry().wrapping_add(bias);
    if !is_user_address(entry) || entry >= user_end(abi) {
        return Err(LoadError::EntryNotUser(entry));
    }
    Ok(entry)
}

/// Load `image` into `space`.
///
/// The space should be empty. Nothing here checks that: a new process's space
/// is fresh, and `execve` empties the running program's space first, after
/// [`check`] has said the image will not be refused for what it is.
///
/// # Errors
///
/// [`LoadError`]. On failure the space may hold part of the image.
pub(crate) fn load(
    space: &AddressSpace,
    image: Source<'_>,
    interpreter: Option<Source<'_>>,
) -> Result<Loaded, LoadError> {
    let (elf, abi) = parse(image)?;
    let (linked_low, _) = elf.load_span(PAGE_SIZE).ok_or(LoadError::Empty)?;
    // The space's ceiling first, so that nothing is placed past it: a 32-bit
    // program's space ends below 4 GiB, and `mmap`'s search stops there too.
    space.set_ceiling(user_end(abi))?;
    let program = place(
        space,
        &elf,
        bias_of(&elf, linked_low, interpreter.is_some(), abi)?,
        image,
        abi,
    )?;

    // The auxiliary vector always describes the *program*: its headers, its
    // entry. What changes when there is a linker is only where the processor
    // starts, and that `AT_BASE` says where the linker went.
    let mut loaded = Loaded {
        entry: program.entry,
        start: program.entry,
        phdr: program.phdr,
        phent: u64::from(elf.header().phentsize),
        phnum: u64::from(elf.header().phnum),
        end: program.high,
        base: 0,
        abi,
    };

    if let Some(source) = interpreter {
        let (linker, _) = parse_linker(source, abi)?;
        let (low, _) = linker.load_span(PAGE_SIZE).ok_or(LoadError::Empty)?;
        let placed = place(
            space,
            &linker,
            interp_base(abi).wrapping_sub(low),
            source,
            abi,
        )?;
        // Only where execution begins changes. `entry` stays the program's,
        // because that is what the linker is told to jump to.
        loaded.start = placed.entry;
        loaded.base = placed.low;
    }

    Ok(loaded)
}

/// One image, mapped into `space` moved by `bias`.
#[derive(Debug, Clone, Copy)]
struct Placed {
    /// Its entry point, moved.
    entry: u64,
    /// Its lowest page, moved: what `AT_BASE` is for a linker.
    low: u64,
    /// One past its highest page, moved.
    high: u64,
    /// Where its program headers landed, or zero.
    phdr: u64,
}

/// A loadable segment, moved by its image's bias.
#[derive(Debug, Clone, Copy)]
struct Moved {
    /// Where it starts.
    at: u64,
    /// Where its file contents are in the file.
    offset: u64,
    /// How many bytes of it come from the file.
    filesz: u64,
    /// How many bytes it occupies.
    memsz: u64,
    /// `PF_*`.
    flags: u32,
}

impl Moved {
    fn of(segment: &Segment, bias: u64) -> Moved {
        Moved {
            at: segment.vaddr.wrapping_add(bias),
            offset: segment.offset,
            filesz: segment.filesz,
            memsz: segment.memsz,
            flags: segment.flags,
        }
    }

    /// The pages it touches, `[first, end)`: every page any byte of it is on.
    fn pages(&self) -> (u64, u64) {
        (
            page_down(self.at),
            page_up(self.at.wrapping_add(self.memsz)),
        )
    }

    /// Where its file contents end in memory.
    fn file_end(&self) -> u64 {
        self.at.wrapping_add(self.filesz)
    }
}

/// Pages mapped from the file: `[start, end)`, from byte `offset` of the
/// file's object, with the permissions of the one segment they belong to.
#[derive(Debug, Clone, Copy)]
struct FileRun {
    start: u64,
    end: u64,
    offset: u64,
    flags: u32,
}

/// Map `elf`'s loadable segments into `space`, moved by `bias`: from the file
/// where `source` has an object to map, and copied in where it has not.
fn place(
    space: &AddressSpace,
    elf: &Elf<'_>,
    bias: u64,
    source: Source<'_>,
    abi: Abi,
) -> Result<Placed, LoadError> {
    let (linked_low, linked_high) = elf.load_span(PAGE_SIZE).ok_or(LoadError::Empty)?;
    let entry = entry_point(elf, bias, abi)?;

    let low = linked_low.wrapping_add(bias);
    let high = linked_high.wrapping_add(bias);
    let _ = high.checked_sub(low).ok_or(LoadError::Empty)?;

    let segments: Vec<Moved> = elf
        .loadable()
        .map(|segment| Moved::of(&segment, bias))
        .collect();
    let runs = match source {
        Source::File(file) => file
            .object()
            .map(|(_, base)| file_runs(&segments, base))
            .unwrap_or_default(),
        Source::Bytes(_) => Vec::new(),
    };

    // Everything the file does not supply is anonymous, and writable for
    // now, so that the copies below have somewhere to land whatever the
    // final permissions turn out to be.
    let anonymous = outside(low, high, &runs);
    for &(start, end) in &anonymous {
        let _ = space.map_anonymous(start, end - start, VmaFlags::READ_WRITE)?;
    }
    if let Source::File(file) = source {
        map_runs(space, file, &runs)?;
        zero_tails(space, &segments, &runs)?;
    }

    // Each segment's file contents, less what the file's own pages show. The
    // rest of `p_memsz` is `.bss`, and is already zero.
    let mut buffer = Vec::new();
    for segment in &segments {
        for (start, end) in outside(segment.at, segment.file_end(), &runs) {
            let from = segment.offset.wrapping_add(start - segment.at);
            copy_in(space, source, from, start, end - start, &mut buffer)?;
        }
    }

    apply_permissions(space, &segments, &anonymous)?;

    Ok(Placed {
        entry,
        low,
        high,
        phdr: program_headers_at(elf, bias),
    })
}

/// The pages of `segments` that can be mapped from a file whose object holds
/// its first byte at `base`, lowest first.
///
/// A page qualifies when it holds some of one segment's file contents, is in
/// no other segment's pages, and that segment's file offset and address agree
/// within a page -- which `ld` and `lld` both guarantee, and without which no
/// mapping can show the file's bytes at the addresses the headers ask for.
/// The page a segment starts on and the page its file contents end on are
/// mapped whole, as Linux's `elf_map` maps them: they show the file's bytes
/// around the segment, and every page of the image carries the file's name in
/// `/proc/<pid>/maps`, which NVIDIA's Vulkan driver reads to find the
/// program. A writable segment's `.bss` tail on its last file page is zeroed
/// after the mapping ([`zero_tails`]); a read-only segment whose memory runs
/// past its file contents keeps that last page anonymous, since nothing may
/// write it to zero it.
fn file_runs(segments: &[Moved], base: u64) -> Vec<FileRun> {
    let mut runs = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if segment.filesz == 0
            || !segment
                .at
                .wrapping_sub(segment.offset)
                .is_multiple_of(PAGE_SIZE)
        {
            continue;
        }
        let writable = segment.flags & PF_W != 0;
        let mem_end = segment.at.wrapping_add(segment.memsz);
        let end = if writable || mem_end <= segment.file_end() {
            page_up(segment.file_end())
        } else {
            page_down(segment.file_end())
        };
        let mut pieces = vec![(page_down(segment.at), end)];
        for (_, other) in segments.iter().enumerate().filter(|(at, _)| *at != index) {
            let (first, end) = other.pages();
            pieces = without(&pieces, first, end);
        }
        for (start, end) in pieces.into_iter().filter(|(start, end)| start < end) {
            let offset = segment
                .offset
                .checked_add(start)
                .and_then(|offset| offset.checked_sub(segment.at))
                .and_then(|offset| offset.checked_add(base));
            if let Some(offset) = offset {
                runs.push(FileRun {
                    start,
                    end,
                    offset,
                    flags: segment.flags,
                });
            }
        }
    }
    runs.sort_by_key(|run| run.start);
    runs
}

/// Map each of `runs` from `file`'s object, privately: a write copies the
/// page into the mapping's own object and never reaches the file.
fn map_runs(space: &AddressSpace, file: &ProgramFile, runs: &[FileRun]) -> Result<(), LoadError> {
    let Some((vmo, _)) = file.object() else {
        return Ok(());
    };
    for run in runs {
        if run.flags & PF_W != 0 && run.flags & PF_X != 0 {
            return Err(LoadError::WriteExecute(run.start));
        }
        let mapping = FileMapping {
            file: Arc::clone(file.file()) as Arc<dyn Any + Send + Sync>,
            may_write: false,
        };
        let _ = space.map_file(
            FilePlace::Fixed(run.start),
            run.end - run.start,
            permissions(run.flags),
            Arc::clone(vmo),
            run.offset,
            mapping,
        )?;
    }
    Ok(())
}

/// Zero each writable segment's `.bss` where it shares a page mapped from the
/// file with the segment's last file bytes: the mapping is private, so the
/// write copies that page, and the file never sees it. Linux's `padzero`.
///
/// The page is filled from the file alone first
/// ([`AddressSpace::fill_file_page_alone`]): the write's fault would read
/// ahead a run of the pages after it, which for a data segment followed by a
/// large one are pages of the program nothing touches, and loading must
/// read only what it uses (`fs/exec_check.rs`, `LOAD_MOST`; FX-0871).
fn zero_tails(space: &AddressSpace, segments: &[Moved], runs: &[FileRun]) -> Result<(), LoadError> {
    const ZEROS: [u8; 4096] = [0; 4096];
    for segment in segments.iter().filter(|segment| segment.flags & PF_W != 0) {
        let mem_end = segment.at.wrapping_add(segment.memsz);
        for run in runs {
            let from = run.start.max(segment.file_end());
            let to = run.end.min(mem_end);
            if from < to && run.start < segment.file_end() {
                let len = usize::try_from(to - from)
                    .map_err(|_| LoadError::Malformed(ElfError::SegmentOutOfBounds))?;
                let zeros = ZEROS
                    .get(..len)
                    .ok_or(LoadError::Malformed(ElfError::SegmentOutOfBounds))?;
                space.fill_file_page_alone(from)?;
                uaccess::copy_to_user(space, from, zeros)?;
            }
        }
    }
    Ok(())
}

/// Copy `len` bytes from byte `from` of the image into `space` at `to`.
///
/// Bytes in memory are copied as they are; a file is read through `buffer`
/// a piece at a time, so that a file of any size is copied without being held
/// whole.
fn copy_in(
    space: &AddressSpace,
    source: Source<'_>,
    from: u64,
    to: u64,
    len: u64,
    buffer: &mut Vec<u8>,
) -> Result<(), LoadError> {
    let file = match source {
        Source::Bytes(bytes) => {
            let data = usize::try_from(from)
                .ok()
                .zip(usize::try_from(len).ok())
                .and_then(|(from, len)| bytes.get(from..from.checked_add(len)?))
                .ok_or(LoadError::Malformed(ElfError::SegmentOutOfBounds))?;
            uaccess::copy_to_user(space, to, data)?;
            return Ok(());
        }
        Source::File(file) => file,
    };
    if buffer.is_empty() {
        *buffer = program::copy_buffer().map_err(LoadError::Read)?;
    }
    let mut done = 0;
    while done < len {
        let take = usize::try_from(len - done)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let chunk = buffer
            .get_mut(..take)
            .ok_or(LoadError::Read(Errno::ENOMEM))?;
        file.read_exact_at(from + done, chunk)
            .map_err(LoadError::Read)?;
        uaccess::copy_to_user(space, to + done, chunk)?;
        done += take as u64;
    }
    Ok(())
}

/// Give every anonymous page the permissions the segments covering it ask
/// for; a page mapped from the file has its segment's already.
fn apply_permissions(
    space: &AddressSpace,
    segments: &[Moved],
    anonymous: &[(u64, u64)],
) -> Result<(), LoadError> {
    for &(start, end) in anonymous {
        // Where the answer can change: a segment's first or last page.
        let mut cuts = vec![start, end];
        for segment in segments {
            let (first, last) = segment.pages();
            cuts.extend(
                [first, last]
                    .into_iter()
                    .filter(|&at| start < at && at < end),
            );
        }
        cuts.sort_unstable();
        cuts.dedup();

        // Runs of pages that agree become one `protect` call each, which is
        // also what keeps the region map from growing a region per page.
        let mut run: Option<(u64, u32)> = None;
        for pair in cuts.windows(2) {
            let &[from, to] = pair else {
                continue;
            };
            let flags = segments
                .iter()
                .filter(|segment| {
                    let (first, last) = segment.pages();
                    first < to && from < last
                })
                .fold(0, |flags, segment| flags | segment.flags);
            match run {
                Some((_, same)) if same == flags => {}
                Some((at, previous)) => {
                    protect(space, at, from, previous)?;
                    run = Some((from, flags));
                }
                None => run = Some((from, flags)),
            }
        }
        if let Some((at, flags)) = run {
            protect(space, at, end, flags)?;
        }
    }
    Ok(())
}

/// Give `[start, end)` the permissions `flags` asks for, refusing a page that
/// would be writable and executable.
fn protect(space: &AddressSpace, start: u64, end: u64, flags: u32) -> Result<(), LoadError> {
    if flags & PF_W != 0 && flags & PF_X != 0 {
        return Err(LoadError::WriteExecute(start));
    }
    space.protect(start, end - start, permissions(flags))?;
    Ok(())
}

/// `[start, end)` less every one of `runs`, which are sorted: the pieces left.
fn outside(start: u64, end: u64, runs: &[FileRun]) -> Vec<(u64, u64)> {
    runs.iter()
        .fold(vec![(start, end)], |pieces, run| {
            without(&pieces, run.start, run.end)
        })
        .into_iter()
        .filter(|(start, end)| start < end)
        .collect()
}

/// `pieces` less `[cut, cut_end)`.
fn without(pieces: &[(u64, u64)], cut: u64, cut_end: u64) -> Vec<(u64, u64)> {
    let mut left = Vec::with_capacity(pieces.len() + 1);
    for &(start, end) in pieces {
        if cut_end <= start || end <= cut {
            left.push((start, end));
            continue;
        }
        if start < cut {
            left.push((start, cut));
        }
        if cut_end < end {
            left.push((cut_end, end));
        }
    }
    left
}

/// `address`, rounded down to its page.
const fn page_down(address: u64) -> u64 {
    address & !(PAGE_SIZE - 1)
}

/// `address`, rounded up to a page; the last page for one that would wrap.
const fn page_up(address: u64) -> u64 {
    match address.checked_add(PAGE_SIZE - 1) {
        Some(up) => page_down(up),
        None => page_down(u64::MAX),
    }
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

/// Where the program header table ended up in memory.
///
/// musl reads it to find its own `PT_TLS` and `PT_GNU_RELRO`, so getting it
/// wrong is not cosmetic. The table is at a file offset; the address is that
/// offset translated through whichever loadable segment's file range contains
/// it, and moved as the image was. Zero if none does, which is what Linux
/// reports in the same case. A static PIE's start finds its own base from
/// this, so for one it has to be right to the byte.
fn program_headers_at(elf: &Elf<'_>, bias: u64) -> u64 {
    let phoff = elf.header().phoff;
    for segment in elf.loadable() {
        let Some(end) = segment.offset.checked_add(segment.filesz) else {
            continue;
        };
        if phoff >= segment.offset && phoff < end {
            return segment
                .vaddr
                .saturating_add(phoff - segment.offset)
                .wrapping_add(bias);
        }
    }
    0
}
