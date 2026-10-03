//! Loading RM's core, `nvrm-core`, from the NVIDIA volume
//! (`docs/NVIDIA.md` §4.1, "The core").
//!
//! RM's core is 12 MB of code and read-only data, more than the kernel
//! reads into a driver's image (`docs/DEVMGR.md` §5), so nvrm is started
//! without it and reads it at run time. The core was linked on the host
//! alone, at [`BASE`], against nvrm's own addresses; its first bytes are an
//! export header with nvrm's build-id and the address of everything nvrm
//! calls in it. Loading it is:
//!
//! 1. read the whole file into memory of nvrm's own;
//! 2. [`check`] it, before any of it is mapped: the pin, the ELF shape,
//!    every segment below 2 GiB and none both writable and executable, the
//!    export header and every export's place;
//! 3. map anonymous memory at each segment's address, never over anything
//!    (`MAP_FIXED_NOREPLACE`; `EEXIST` is a refusal, not a retry
//!    elsewhere), copy the segment in, and fill nvrm's call table;
//! 4. give each segment its final protection, and then read
//!    `/proc/self/maps` back ([`maps_check`]): nothing in the core's range
//!    writable and executable, text `r-x`, read-only data `r--`.
//!
//! The kernel does not refuse a mapping that is writable and executable --
//! Linux does not either -- so that discipline is nvrm's, and step 4 is
//! where nvrm holds itself to it. Each refusal is its own line, and nvrm
//! then exits: there is no fallback.

use core::ffi::{c_char, c_int, c_void};
use core::fmt;

use crate::{libc, log, sha256};

/// `"NVRMCORE"`, little-endian: the export header's first word.
pub const MAGIC: u64 = 0x4552_4f43_4d52_564e;
/// The export header's layout.
pub const VERSION: u32 = 1;
/// Where the core is linked (`core/nvrm-core.ld`).
pub const BASE: u64 = 0x4000_0000;
/// The end of the low 2 GiB, which the core's `R_X86_64_32S` relocations
/// reach and nothing of it may pass.
pub const END: u64 = 0x8000_0000;
/// The largest core file read.
pub const MAX_FILE: usize = 64 << 20;

/// A page.
const PAGE: u64 = 4096;
/// The most loadable segments a core has (`core/nvrm-core.ld` makes three).
const MAX_SEGMENTS: usize = 4;
/// The export header before its entries: magic, version, count, the
/// build-id and its length.
const HEADER_BYTES: usize = 40;
/// A sha1 build-id's length.
const BUILD_ID_BYTES: usize = 20;

/// `PT_LOAD`.
const PT_LOAD: u32 = 1;
/// `PT_NOTE`.
const PT_NOTE: u32 = 4;
/// `PT_GNU_STACK`.
const PT_GNU_STACK: u32 = 0x6474_e551;
/// `PF_X`.
const PF_X: u32 = 1;
/// `PF_W`.
const PF_W: u32 = 2;
/// `PF_R`.
const PF_R: u32 = 4;
/// `NT_GNU_BUILD_ID`.
const NT_GNU_BUILD_ID: u32 = 3;

/// Why a core was refused. Each is one line, and its own exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// nvrm's pin is the build's all-zero placeholder: it was never set.
    Unpinned,
    /// nvrm has no build-id note of its own.
    NoBuildId,
    /// The file could not be opened.
    Open(c_int),
    /// The file could not be read whole.
    Read(c_int),
    /// The file is empty or larger than [`MAX_FILE`].
    Size(usize),
    /// The file's sha256 is not nvrm's pin.
    Hash,
    /// Not a 64-bit little-endian x86-64 `ET_EXEC` with whole program
    /// headers.
    NotElf,
    /// A program header other than `PT_LOAD` and `PT_GNU_STACK`, an
    /// executable stack, no segments or too many.
    Shape,
    /// A segment not page-aligned, with more file than memory, or past the
    /// file's end.
    Segment,
    /// A segment both writable and executable.
    WriteExecute,
    /// A segment outside `[BASE, END)`.
    Range,
    /// Segments out of order or overlapping.
    Overlap,
    /// The export header's magic is wrong, or it is not at the base.
    Magic,
    /// The export header's version is not [`VERSION`].
    Version,
    /// The export header's count is not nvrm's.
    Count,
    /// The core was linked against another build of nvrm.
    BuildId,
    /// An export outside its segment: a function not in executable
    /// memory, or data in it.
    Export,
    /// Something already maps a segment's place.
    Taken,
    /// `mmap` refused a segment.
    Map(c_int),
    /// `mprotect` refused a segment.
    Protect(c_int),
    /// `/proc/self/maps` could not be read.
    Maps(c_int),
    /// `/proc/self/maps` shows a writable and executable mapping in the
    /// core's range.
    MapsWriteExecute,
    /// `/proc/self/maps` shows a segment with other than its protection,
    /// not wholly mapped, or something else in the core's range.
    MapsProtection,
}

impl Refusal {
    /// The exit status nvrm ends with.
    pub fn code(self) -> c_int {
        match self {
            Refusal::Unpinned => 20,
            Refusal::NoBuildId => 21,
            Refusal::Open(_) => 22,
            Refusal::Read(_) => 23,
            Refusal::Size(_) => 24,
            Refusal::Hash => 25,
            Refusal::NotElf => 26,
            Refusal::Shape => 27,
            Refusal::Segment => 28,
            Refusal::WriteExecute => 29,
            Refusal::Range => 30,
            Refusal::Overlap => 31,
            Refusal::Magic => 32,
            Refusal::Version => 33,
            Refusal::Count => 34,
            Refusal::BuildId => 35,
            Refusal::Export => 36,
            Refusal::Taken => 37,
            Refusal::Map(_) => 38,
            Refusal::Protect(_) => 39,
            Refusal::Maps(_) => 40,
            Refusal::MapsWriteExecute => 41,
            Refusal::MapsProtection => 42,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refusal::Unpinned => out.write_str("nvrm's core pin is unset (all zero)"),
            Refusal::NoBuildId => out.write_str("nvrm has no build-id of its own"),
            Refusal::Open(errno) => write!(out, "the core cannot be opened (errno {errno})"),
            Refusal::Read(errno) => write!(out, "the core cannot be read (errno {errno})"),
            Refusal::Size(size) => write!(out, "the core is {size} bytes, not 1 to {MAX_FILE}"),
            Refusal::Hash => out.write_str("the core's sha256 is not nvrm's pin"),
            Refusal::NotElf => out.write_str("the core is not an x86-64 ELF executable"),
            Refusal::Shape => out.write_str(
                "the core has a program header other than PT_LOAD and PT_GNU_STACK, \
                 an executable stack, or no or too many segments",
            ),
            Refusal::Segment => out.write_str("a segment of the core is malformed"),
            Refusal::WriteExecute => {
                out.write_str("a segment of the core is writable and executable")
            }
            Refusal::Range => write!(
                out,
                "a segment of the core is outside [{BASE:#x}, {END:#x})"
            ),
            Refusal::Overlap => out.write_str("the core's segments overlap or are out of order"),
            Refusal::Magic => out.write_str("the core's export header has the wrong magic"),
            Refusal::Version => out.write_str("the core's export header has another version"),
            Refusal::Count => out.write_str("the core exports another count than nvrm calls"),
            Refusal::BuildId => out.write_str("the core was linked against another nvrm build"),
            Refusal::Export => out.write_str("an export of the core lies outside its segment"),
            Refusal::Taken => out.write_str("a segment's place is already mapped (EEXIST)"),
            Refusal::Map(errno) => write!(out, "mmap refused a segment (errno {errno})"),
            Refusal::Protect(errno) => write!(out, "mprotect refused a segment (errno {errno})"),
            Refusal::Maps(errno) => write!(out, "/proc/self/maps cannot be read (errno {errno})"),
            Refusal::MapsWriteExecute => out.write_str(
                "/proc/self/maps shows a writable and executable mapping in the core's range",
            ),
            Refusal::MapsProtection => out.write_str(
                "/proc/self/maps shows a segment without its protection, or a stranger in the \
                 core's range",
            ),
        }
    }
}

/// A loadable segment, as checked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Segment {
    /// Its address, page-aligned.
    pub address: u64,
    /// Its size in memory.
    pub memory: u64,
    /// Where its bytes start in the file.
    pub offset: u64,
    /// How many bytes come from the file.
    pub file: u64,
    /// `PF_*`.
    pub flags: u32,
}

impl Segment {
    /// The page-rounded end.
    fn end(&self) -> u64 {
        self.address
            .saturating_add(self.memory)
            .next_multiple_of(PAGE)
    }

    /// Whether `[address, address + 8)` lies inside.
    fn holds(&self, address: u64) -> bool {
        address >= self.address && address.saturating_add(8) <= self.address + self.memory
    }

    /// The permissions `/proc/self/maps` shows for it.
    fn perms(&self) -> &'static [u8; 3] {
        if self.flags & PF_X != 0 {
            b"r-x"
        } else if self.flags & PF_W != 0 {
            b"rw-"
        } else {
            b"r--"
        }
    }

    /// Its protection for `mprotect`.
    fn protection(&self) -> c_int {
        let mut protection = 0;
        if self.flags & PF_R != 0 {
            protection |= libc::PROT_READ;
        }
        if self.flags & PF_W != 0 {
            protection |= libc::PROT_WRITE;
        }
        if self.flags & PF_X != 0 {
            protection |= libc::PROT_EXEC;
        }
        protection
    }
}

/// A checked core: its segments, in address order, and where its export
/// entries start in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The segments.
    pub segments: [Segment; MAX_SEGMENTS],
    /// How many of them there are.
    pub count: usize,
    /// The file offset of the first export entry.
    pub entries: usize,
    /// How many there are.
    pub exports: u32,
}

impl Plan {
    /// The segments there are.
    pub fn segments(&self) -> &[Segment] {
        self.segments.get(..self.count).unwrap_or_default()
    }
}

/// A little-endian integer of `N` bytes at `at`, if the file holds it.
fn le<const N: usize>(file: &[u8], at: u64) -> Option<u64> {
    let at = usize::try_from(at).ok()?;
    let bytes = file.get(at..at.checked_add(N)?)?;
    let mut word = [0_u8; 8];
    word.get_mut(..N)?.copy_from_slice(bytes);
    Some(u64::from_le_bytes(word))
}

/// The program header at `at`: (type, flags, offset, address, file size,
/// memory size).
fn program_header(file: &[u8], at: u64) -> Option<(u32, u32, u64, u64, u64, u64)> {
    Some((
        u32::try_from(le::<4>(file, at)?).ok()?,
        u32::try_from(le::<4>(file, at + 4)?).ok()?,
        le::<8>(file, at + 8)?,
        le::<8>(file, at + 16)?,
        le::<8>(file, at + 32)?,
        le::<8>(file, at + 40)?,
    ))
}

/// Check `file`, the whole core, against nvrm's `pin` and `build_id`, for
/// `exports` entries of which the first `functions` are code. Nothing is
/// mapped or changed.
///
/// # Errors
///
/// The first [`Refusal`] that applies, in the order of the list in the
/// module's head.
pub fn check(
    file: &[u8],
    pin: &[u8; 32],
    build_id: &[u8],
    exports: u32,
    functions: u32,
) -> Result<Plan, Refusal> {
    if pin.iter().all(|&byte| byte == 0) {
        return Err(Refusal::Unpinned);
    }
    if file.is_empty() || file.len() > MAX_FILE {
        return Err(Refusal::Size(file.len()));
    }
    if &sha256::digest(file) != pin {
        return Err(Refusal::Hash);
    }
    let plan = segments(file)?;
    exports_check(file, plan, build_id, exports, functions)
}

/// The ELF header and the program headers.
fn segments(file: &[u8]) -> Result<Plan, Refusal> {
    let ident_ok = file.get(..7) == Some(&[0x7f, b'E', b'L', b'F', 2, 1, 1][..]);
    let header = (
        le::<2>(file, 16),
        le::<2>(file, 18),
        le::<8>(file, 32),
        le::<2>(file, 54),
        le::<2>(file, 56),
    );
    let (Some(2), Some(62), Some(phoff), Some(56), Some(phnum)) = header else {
        return Err(Refusal::NotElf);
    };
    if !ident_ok {
        return Err(Refusal::NotElf);
    }
    let mut plan = Plan {
        segments: [Segment::default(); MAX_SEGMENTS],
        count: 0,
        entries: 0,
        exports: 0,
    };
    for index in 0..phnum {
        let at = phoff.checked_add(index * 56).ok_or(Refusal::NotElf)?;
        let (kind, flags, offset, address, filesz, memsz) =
            program_header(file, at).ok_or(Refusal::NotElf)?;
        if kind == PT_GNU_STACK {
            if flags & PF_X != 0 {
                return Err(Refusal::Shape);
            }
            continue;
        }
        if kind != PT_LOAD {
            return Err(Refusal::Shape);
        }
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(Refusal::WriteExecute);
        }
        let in_file = offset
            .checked_add(filesz)
            .is_some_and(|end| end <= file.len() as u64);
        if !address.is_multiple_of(PAGE) || filesz > memsz || memsz == 0 || !in_file {
            return Err(Refusal::Segment);
        }
        let end = address.checked_add(memsz).ok_or(Refusal::Range)?;
        if address < BASE || end > END {
            return Err(Refusal::Range);
        }
        let segment = Segment {
            address,
            memory: memsz,
            offset,
            file: filesz,
            flags,
        };
        if let Some(previous) = plan.segments().last()
            && previous.end() > segment.address
        {
            return Err(Refusal::Overlap);
        }
        let slot = plan.segments.get_mut(plan.count).ok_or(Refusal::Shape)?;
        *slot = segment;
        plan.count += 1;
    }
    if plan.count == 0 {
        return Err(Refusal::Shape);
    }
    Ok(plan)
}

/// The export header, at the base, in the first segment, and every entry.
fn exports_check(
    file: &[u8],
    mut plan: Plan,
    build_id: &[u8],
    exports: u32,
    functions: u32,
) -> Result<Plan, Refusal> {
    let first = *plan.segments().first().ok_or(Refusal::Shape)?;
    if first.address != BASE || first.flags & (PF_W | PF_X) != 0 {
        return Err(Refusal::Magic);
    }
    let at = first.offset;
    if le::<8>(file, at) != Some(MAGIC) {
        return Err(Refusal::Magic);
    }
    if le::<4>(file, at + 8) != Some(u64::from(VERSION)) {
        return Err(Refusal::Version);
    }
    if le::<4>(file, at + 12) != Some(u64::from(exports)) {
        return Err(Refusal::Count);
    }
    let id_at = usize::try_from(at + 16).map_err(|_| Refusal::Magic)?;
    let recorded = file.get(id_at..id_at + BUILD_ID_BYTES);
    if le::<4>(file, at + 36) != Some(BUILD_ID_BYTES as u64) || recorded != Some(build_id) {
        return Err(Refusal::BuildId);
    }
    let table = (HEADER_BYTES as u64) + 8 * u64::from(exports);
    if first.file < table {
        return Err(Refusal::Count);
    }
    for index in 0..exports {
        let address = le::<8>(file, at + HEADER_BYTES as u64 + 8 * u64::from(index))
            .ok_or(Refusal::Export)?;
        let code = index < functions;
        let placed = plan
            .segments()
            .iter()
            .any(|segment| segment.holds(address) && (segment.flags & PF_X != 0) == code);
        if !placed {
            return Err(Refusal::Export);
        }
    }
    plan.entries = usize::try_from(at).map_err(|_| Refusal::Export)? + HEADER_BYTES;
    plan.exports = exports;
    Ok(plan)
}

/// One line of `/proc/self/maps`: its range and its first three
/// permission letters.
fn maps_line(line: &[u8]) -> Option<(u64, u64, [u8; 3])> {
    let mut fields = line.split(|&byte| byte == b' ');
    let range = fields.next()?;
    let perms = fields.next()?;
    let mut ends = range.split(|&byte| byte == b'-');
    let hex = |text: &[u8]| u64::from_str_radix(core::str::from_utf8(text).ok()?, 16).ok();
    let start = hex(ends.next()?)?;
    let end = hex(ends.next()?)?;
    let mut three = [0_u8; 3];
    three.copy_from_slice(perms.get(..3)?);
    Some((start, end, three))
}

/// Hold `/proc/self/maps`'s text to the plan: in `[BASE, END)`, nothing
/// writable and executable, every byte of every segment mapped with its
/// own permissions, and nothing else.
///
/// # Errors
///
/// [`Refusal::MapsWriteExecute`] or [`Refusal::MapsProtection`].
pub fn maps_check(maps: &[u8], plan: &Plan) -> Result<(), Refusal> {
    let mut covered = [0_u64; MAX_SEGMENTS];
    for line in maps
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let (start, end, perms) = maps_line(line).ok_or(Refusal::MapsProtection)?;
        if end <= BASE || start >= END {
            continue;
        }
        if perms[1] == b'w' && perms[2] == b'x' {
            return Err(Refusal::MapsWriteExecute);
        }
        let mut owner = None;
        for (index, segment) in plan.segments().iter().enumerate() {
            if start >= segment.address && end <= segment.end() {
                owner = Some((index, segment));
            }
        }
        let Some((index, segment)) = owner else {
            return Err(Refusal::MapsProtection);
        };
        if &perms != segment.perms() {
            return Err(Refusal::MapsProtection);
        }
        if let Some(sum) = covered.get_mut(index) {
            *sum += end - start;
        }
    }
    for (segment, &sum) in plan.segments().iter().zip(covered.iter()) {
        if sum != segment.end() - segment.address {
            return Err(Refusal::MapsProtection);
        }
    }
    Ok(())
}

/// The descriptor of an `NT_GNU_BUILD_ID` note in `notes`, a `PT_NOTE`
/// segment's bytes.
pub fn build_id_in(notes: &[u8]) -> Option<&[u8]> {
    let mut at = 0_usize;
    while at + 12 <= notes.len() {
        let word = |offset: usize| le::<4>(notes, (at + offset) as u64);
        let name = usize::try_from(word(0)?).ok()?;
        let desc = usize::try_from(word(4)?).ok()?;
        let kind = word(8)?;
        let name_at = at + 12;
        let desc_at = name_at + name.next_multiple_of(4);
        let next = desc_at + desc.next_multiple_of(4);
        if kind == u64::from(NT_GNU_BUILD_ID)
            && notes.get(name_at..name_at + name) == Some(b"GNU\0")
        {
            return notes.get(desc_at..desc_at + desc);
        }
        at = next;
    }
    None
}

/// nvrm's own build-id, from its program headers.
fn own_build_id() -> Option<&'static [u8]> {
    unsafe extern "C" {
        /// The ELF header, where the linker put it: nvrm's first byte.
        static __ehdr_start: [u8; 64];
    }
    // SAFETY: the linker defines `__ehdr_start` at the mapped ELF header of
    // this static, non-PIE program, which stays mapped for its life.
    let header: &'static [u8; 64] = unsafe { &__ehdr_start };
    let phoff = usize::try_from(le::<8>(header, 32)?).ok()?;
    let phnum = usize::try_from(le::<2>(header, 56)?).ok()?;
    let base = header.as_ptr();
    // SAFETY: the program headers are mapped with the ELF header, in the
    // first loaded page, `phnum` entries of 56 bytes at `phoff`.
    let headers = unsafe { core::slice::from_raw_parts(base.wrapping_add(phoff), phnum * 56) };
    for index in 0..phnum {
        let (kind, _, _, address, size, _) = program_header(headers, (index * 56) as u64)?;
        if kind == PT_NOTE {
            let address = usize::try_from(address).ok()?;
            let size = usize::try_from(size).ok()?;
            // SAFETY: a non-PIE program's PT_NOTE is mapped at its own
            // address, read-only, for the program's life.
            let notes = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
            if let Some(id) = build_id_in(notes) {
                return Some(id);
            }
        }
    }
    None
}

/// The current `errno`, as a refusal's detail.
fn errno() -> c_int {
    libc::errno()
}

/// Read all of `fd` into fresh anonymous memory. Answers its address and
/// length; the caller unmaps it.
fn read_whole(fd: c_int) -> Result<(*mut u8, usize), Refusal> {
    // SAFETY: `lseek` on a descriptor this function's caller opened.
    let size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    let size = usize::try_from(size).map_err(|_| Refusal::Read(errno()))?;
    if size == 0 || size > MAX_FILE {
        return Err(Refusal::Size(size));
    }
    // SAFETY: a fresh private anonymous mapping, placed by the kernel.
    let buffer = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ_WRITE,
            libc::MAP_PRIVATE_ANONYMOUS,
            -1,
            0,
        )
    };
    if buffer == libc::MAP_FAILED {
        return Err(Refusal::Read(errno()));
    }
    let buffer = buffer.cast::<u8>();
    let mut done = 0_usize;
    while done < size {
        // SAFETY: `buffer` holds `size` bytes, of which `done` are read.
        let got = unsafe {
            libc::pread(
                fd,
                buffer.wrapping_add(done).cast(),
                size - done,
                done as i64,
            )
        };
        match usize::try_from(got) {
            Ok(0) | Err(_) => {
                // SAFETY: the mapping made above, of `size` bytes.
                let _ = unsafe { libc::munmap(buffer.cast(), size) };
                return Err(Refusal::Read(errno()));
            }
            Ok(got) => done += got,
        }
    }
    Ok((buffer, size))
}

/// Read `/proc/self/maps` whole into `out`; answers how much it holds.
fn read_maps(out: &mut [u8]) -> Result<usize, Refusal> {
    // SAFETY: a NUL-terminated path.
    let fd = unsafe {
        libc::open(
            c"/proc/self/maps".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Refusal::Maps(errno()));
    }
    let mut done = 0_usize;
    loop {
        let rest = out.get_mut(done..).unwrap_or_default();
        if rest.is_empty() {
            // SAFETY: the descriptor opened above.
            let _ = unsafe { libc::close(fd) };
            return Err(Refusal::Maps(0));
        }
        // SAFETY: `rest` is a live, writable slice of `rest.len()` bytes.
        let got = unsafe { libc::read(fd, rest.as_mut_ptr().cast(), rest.len()) };
        match usize::try_from(got) {
            Ok(0) => break,
            Ok(got) => done += got,
            Err(_) => {
                let error = errno();
                // SAFETY: the descriptor opened above.
                let _ = unsafe { libc::close(fd) };
                return Err(Refusal::Maps(error));
            }
        }
    }
    // SAFETY: the descriptor opened above.
    let _ = unsafe { libc::close(fd) };
    Ok(done)
}

/// The room `/proc/self/maps` is read into.
const MAPS_BYTES: usize = 256 << 10;

/// Map the checked `plan` from `file` and fill `table`.
///
/// # Safety
///
/// `table` must be valid for `plan.exports` writes, and nothing of nvrm's
/// may run from `[BASE, END)` meanwhile.
unsafe fn map(file: &[u8], plan: &Plan, table: *mut u64) -> Result<(), Refusal> {
    for segment in plan.segments() {
        let length =
            usize::try_from(segment.end() - segment.address).map_err(|_| Refusal::Range)?;
        let want = segment.address as *mut c_void;
        // SAFETY: a fresh private anonymous mapping at a place that the
        // flags forbid replacing: the kernel refuses rather than unmap.
        let got = unsafe {
            libc::mmap(
                want,
                length,
                libc::PROT_READ_WRITE,
                libc::MAP_PRIVATE_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if got == libc::MAP_FAILED {
            let error = errno();
            return Err(if error == libc::EEXIST {
                Refusal::Taken
            } else {
                Refusal::Map(error)
            });
        }
        if got != want {
            // A kernel that took the flag as a hint placed it elsewhere.
            // SAFETY: the mapping just made, of `length` bytes.
            let _ = unsafe { libc::munmap(got, length) };
            return Err(Refusal::Taken);
        }
        let from = usize::try_from(segment.offset).map_err(|_| Refusal::Segment)?;
        let bytes = usize::try_from(segment.file).map_err(|_| Refusal::Segment)?;
        let source = file.get(from..from + bytes).ok_or(Refusal::Segment)?;
        // SAFETY: `got` is `length` >= `bytes` fresh writable bytes that
        // `source` does not overlap.
        unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), got.cast::<u8>(), bytes) };
    }
    for index in 0..plan.exports as usize {
        let entry = le::<8>(file, (plan.entries + 8 * index) as u64).ok_or(Refusal::Export)?;
        // SAFETY: the caller's `table` holds `plan.exports` slots.
        unsafe { table.wrapping_add(index).write(entry) };
    }
    for segment in plan.segments() {
        let length =
            usize::try_from(segment.end() - segment.address).map_err(|_| Refusal::Range)?;
        // SAFETY: the segment's own mapping, made above.
        let done =
            unsafe { libc::mprotect(segment.address as *mut c_void, length, segment.protection()) };
        if done != 0 {
            return Err(Refusal::Protect(errno()));
        }
    }
    Ok(())
}

/// Load the core at `path` (see the module's head).
fn load(
    path: *const c_char,
    pin: &[u8; 32],
    table: *mut u64,
    exports: u32,
    functions: u32,
) -> Result<(), Refusal> {
    if pin.iter().all(|&byte| byte == 0) {
        return Err(Refusal::Unpinned);
    }
    let build_id = own_build_id().ok_or(Refusal::NoBuildId)?;
    // SAFETY: `path` is the caller's NUL-terminated string.
    let fd = unsafe { libc::open(path, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(Refusal::Open(errno()));
    }
    let read = read_whole(fd);
    // SAFETY: the descriptor opened above.
    let _ = unsafe { libc::close(fd) };
    let (buffer, size) = read?;
    // SAFETY: `read_whole` filled `size` bytes at `buffer`, unmapped below.
    let file = unsafe { core::slice::from_raw_parts(buffer, size) };
    let checked = check(file, pin, build_id, exports, functions);
    // SAFETY: the plan is checked, and the caller gives `table`.
    let mapped = checked.and_then(|plan| unsafe { map(file, &plan, table) }.map(|()| plan));
    // SAFETY: the buffer `read_whole` mapped, `size` bytes, no longer used.
    let _ = unsafe { libc::munmap(buffer.cast(), size) };
    let plan = mapped?;
    verify(&plan)?;
    let digest = pin;
    log::say_line(format_args!(
        "core loaded: sha256 {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}..., {size} bytes, \
         base {BASE:#x}, {exports} exports, build-id {}",
        digest[0],
        digest[1],
        digest[2],
        digest[3],
        digest[4],
        digest[5],
        digest[6],
        digest[7],
        Hex(build_id),
    ));
    Ok(())
}

/// Read `/proc/self/maps` and hold it to `plan`.
fn verify(plan: &Plan) -> Result<(), Refusal> {
    let size = MAPS_BYTES;
    // SAFETY: a fresh private anonymous mapping, placed by the kernel.
    let room = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ_WRITE,
            libc::MAP_PRIVATE_ANONYMOUS,
            -1,
            0,
        )
    };
    if room == libc::MAP_FAILED {
        return Err(Refusal::Maps(errno()));
    }
    // SAFETY: the mapping just made, `size` writable bytes, unmapped below.
    let out = unsafe { core::slice::from_raw_parts_mut(room.cast::<u8>(), size) };
    let result =
        read_maps(out).and_then(|used| maps_check(out.get(..used).unwrap_or_default(), plan));
    // SAFETY: the mapping made above.
    let _ = unsafe { libc::munmap(room, size) };
    result
}

/// Bytes as lowercase hexadecimal.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(out, "{byte:02x}"))
    }
}

/// `nvos_core_load(path, pin, table, exports, functions)`: load RM's core
/// for nvrm's call table (`src/main.c`). Answers 0, or the refusal's exit
/// status after its line.
///
/// # Safety
///
/// `path` is NUL-terminated, `pin` points at 32 bytes, `table` at
/// `exports` writable slots, and no thread of nvrm has started.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_core_load(
    path: *const c_char,
    pin: *const [u8; 32],
    table: *mut u64,
    exports: u32,
    functions: u32,
) -> c_int {
    // SAFETY: the caller's contract.
    let pin = unsafe { &*pin };
    match load(path, pin, table, exports, functions) {
        Ok(()) => 0,
        Err(refusal) => {
            log::say_line(format_args!("core refused: {refusal}"));
            refusal.code()
        }
    }
}

/// The negative control of the maps check (`nvrm-link-test
/// --control-rwx`): map one page writable and executable at the top of the
/// core's range and run the check as [`nvos_core_load`] does. Answers the
/// refusal's status, which must be [`Refusal::MapsWriteExecute`]'s, or 0 if
/// the check let it pass.
#[unsafe(no_mangle)]
pub extern "C" fn nvos_core_control_rwx() -> c_int {
    let page = (END - PAGE) as *mut c_void;
    // SAFETY: a fresh anonymous page at a place no part of nvrm uses, which
    // the flags forbid replacing.
    let got = unsafe {
        libc::mmap(
            page,
            PAGE as usize,
            libc::PROT_READ_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if got != page {
        log::say_line(format_args!("control: the rwx page could not be mapped"));
        return 1;
    }
    let plan = Plan {
        segments: [Segment::default(); MAX_SEGMENTS],
        count: 0,
        entries: 0,
        exports: 0,
    };
    match verify(&plan) {
        Ok(()) => 0,
        Err(refusal) => {
            log::say_line(format_args!("core refused: {refusal}"));
            refusal.code()
        }
    }
}

#[cfg(test)]
mod tests;
