//! The vDSO: a small shared object the kernel maps into every Linux program,
//! whose functions read the clock without entering the kernel.
//!
//! A C library asks the clock through `clock_gettime`, and a browser asks it
//! tens of thousands of times a second: Chrome on Ferrix made about forty
//! thousand of the calls a second, each a trip into the kernel and back to
//! compute a number the program could have computed itself. The kernel's
//! clock is the TSC scaled by a frequency that never changes after boot, and
//! the real-time clock is that plus one offset, so a function in user memory
//! that can read the frequency and the offset gives the same answer the
//! system call gives. glibc and musl look for such functions at the address
//! `AT_SYSINFO_EHDR` gives them, in an ELF image laid out as a shared object,
//! and fall back to the system call when there is none.
//!
//! # What is here
//!
//! [`build`] lays out that image: an ELF header, a loadable segment and a
//! dynamic one, the dynamic symbol table with its System V hash table and
//! version definitions (`LINUX_2.6`, as Linux's vDSO has them), section
//! headers, and the code at [`CODE_AT`]. [`lookup`] finds a symbol in an image
//! the way the C libraries do, for the tests and the kernel's own check.
//!
//! The code is not here. It is machine code, one architecture's, and lives
//! behind the kernel's architecture facade (`src/kernel/src/arch/x86_64/vdso.rs`),
//! which hands it to [`build`] as bytes and offsets. It has to run in any
//! process at whatever address the page lands, so it is written
//! position-independent and finds its data by its own address.
//!
//! # The data page
//!
//! The code reads a page the kernel maps immediately below the image and
//! keeps up to date, `[vvar]` in `/proc/<pid>/maps`: which counter the kernel
//! reads ([`VVAR_MODE`]), its frequency ([`VVAR_COUNTER_HZ`]) and the
//! real-time clock's offset from it ([`VVAR_REALTIME_OFFSET`]). Each is one
//! aligned word, and the one that changes changes with one store, so a read
//! is never torn and needs no sequence count. With [`MODE_SYSCALL`] every
//! function makes the system call it stands for, which is what a kernel
//! whose counter is not the TSC answers with.
//!
//! # The kernel's clock conversion
//!
//! [`counter_nanos`] and [`CounterScale`] are also the kernel's own
//! conversion from counter ticks to nanoseconds (`timer::now_nanos`), kept
//! here so the vDSO and the kernel answer alike; `docs/BACKLOG.md` has the
//! row to move them into a classified core crate.
//!
//! # No allocation
//!
//! [`build`] writes into a page the caller owns and allocates nothing: the
//! kernel builds the image once, into the frame every process maps.

#![no_std]

#[cfg(test)]
extern crate std;

/// Bytes in the image, and in the data page below it: one page each.
pub const IMAGE_BYTES: usize = 4096;

/// Where in the image the code starts. Everything else -- headers, tables,
/// strings -- fits below it, and [`build`] refuses a layout that does not.
pub const CODE_AT: usize = 0x800;

/// The data page's word saying which counter the kernel reads: [`MODE_TSC`]
/// or [`MODE_SYSCALL`].
pub const VVAR_MODE: usize = 0;

/// The data page's word holding the counter's frequency, in hertz.
pub const VVAR_COUNTER_HZ: usize = 8;

/// The data page's word holding the real-time clock's offset from the
/// counter's nanoseconds, a signed 64-bit count of nanoseconds.
pub const VVAR_REALTIME_OFFSET: usize = 16;

/// Every function makes its system call: the kernel's counter is not one the
/// code can read.
pub const MODE_SYSCALL: u64 = 0;

/// The kernel's counter is the TSC: the functions answer with
/// `rdtsc * 1e9 / hz`, as the kernel's own `now_nanos` does.
pub const MODE_TSC: u64 = 1;

/// `ticks` of a `hz` counter in nanoseconds: `ticks * 10^9 / hz`, rounded
/// down, exactly, and `u64::MAX` past what 64 bits hold. The kernel's clock
/// (`timer::now_nanos`) is this, by [`CounterScale`], and so are the
/// [`MODE_TSC`] functions' answers.
///
/// # Two narrow divisions, not one wide one
///
/// The kernel reads the clock several times on every switch and every wake,
/// and the product in 128 bits divided by `hz` is a library call of a hundred
/// cycles or so. Split at whole seconds, the same answer takes two 64-bit
/// divisions the processor does itself: the seconds times 10^9, plus the
/// ticks left over, fewer than a second's, times 10^9 over `hz`, which fits
/// 64 bits for any counter below 18 GHz. Exact, so every other reading of
/// the counter agrees with it to the nanosecond. A counter of zero hertz
/// counts nothing and reads zero.
#[must_use]
pub const fn counter_nanos(ticks: u64, hz: u64) -> u64 {
    const NANOS: u64 = 1_000_000_000;
    if hz == 0 {
        return 0;
    }
    let seconds = ticks / hz;
    let rest = ticks % hz;
    let Some(within) = rest.checked_mul(NANOS) else {
        // A counter past 18 GHz: the wide way, saturating rather than
        // truncating, as a wrong answer that is obviously wrong beats one
        // that looks plausible.
        let scaled = ticks as u128 * NANOS as u128 / hz as u128;
        return if scaled > u64::MAX as u128 {
            u64::MAX
        } else {
            scaled as u64
        };
    };
    seconds.saturating_mul(NANOS).saturating_add(within / hz)
}

/// [`counter_nanos`] for one rate, by multiplication: the rate's reciprocal
/// worked out once, so that a reading costs two multiplications where
/// [`counter_nanos`] costs two 64-bit divisions (`docs/OPAQUE-KERNEL.md`
/// §9.11, Q1, gives what that saved). The kernel's clock reads it at every
/// switch.
///
/// Exact, not an approximation in the way Linux's `cyc2ns` scaling is: every
/// answer equals [`counter_nanos`] of the same ticks and rate, bit for bit,
/// which the host test holds across rates and readings. So the kernel's
/// clock still agrees to the nanosecond with every other reading of the
/// counter, the vDSO's included, which divides.
///
/// # The reciprocal
///
/// The division of any 64-bit `n` by a fixed `d` as a multiplication:
/// Granlund and Montgomery's round-up method ("Division by invariant
/// integers using multiplication", PLDI 1994), in libdivide's unsigned
/// 64-bit form. With `l` the floor of `log2 d`, a magic number `m` just above
/// `2^(64+l) / d` makes `n / d` the high half of `n * m` shifted right by
/// `l`. Where `m` needs a 65th bit, it is kept without it, and the bit is
/// added back as `((n - q) / 2 + q) >> l`, `q` the high half. A power of
/// two is a shift alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterScale {
    /// The rate, in hertz.
    hz: u64,
    /// The magic number, less its 65th bit when `add` is set; zero for a
    /// power of two.
    magic: u64,
    /// The floor of the base-2 logarithm of `hz`.
    shift: u32,
    /// Whether the magic number has a 65th bit to add back.
    add: bool,
}

impl CounterScale {
    /// The reciprocal of `hz`. One 128-bit division, once; zero hertz
    /// scales every reading to zero, as [`counter_nanos`] does.
    #[must_use]
    pub const fn new(hz: u64) -> CounterScale {
        if hz == 0 {
            return CounterScale {
                hz,
                magic: 0,
                shift: 0,
                add: false,
            };
        }
        let shift = 63 - hz.leading_zeros();
        if hz.is_power_of_two() {
            return CounterScale {
                hz,
                magic: 0,
                shift,
                add: false,
            };
        }
        // 2^(64+l) over `hz`, which is below 2^64 because `hz` is above
        // 2^l: the floor and its remainder.
        let numerator = 1_u128 << (64 + shift);
        let proposed = (numerator / hz as u128) as u64;
        let remainder = (numerator % hz as u128) as u64;
        if hz - remainder < 1_u64 << shift {
            // The floor plus one is within the error the shift by `l`
            // absorbs for every 64-bit `n`.
            CounterScale {
                hz,
                magic: proposed.wrapping_add(1),
                shift,
                add: false,
            }
        } else {
            // It is not: 2^(65+l) over `hz`, rounded up, whose 65th bit is
            // set and is added back in `quotient`.
            let mut doubled = proposed.wrapping_add(proposed);
            let twice = remainder.wrapping_add(remainder);
            if twice >= hz || twice < remainder {
                doubled = doubled.wrapping_add(1);
            }
            CounterScale {
                hz,
                magic: doubled.wrapping_add(1),
                shift,
                add: true,
            }
        }
    }

    /// The rate this is the reciprocal of.
    #[must_use]
    pub const fn hz(&self) -> u64 {
        self.hz
    }

    /// `n / hz`, rounded down, for any `n`; not called with a zero rate.
    const fn quotient(&self, n: u64) -> u64 {
        if self.magic == 0 {
            return n >> self.shift;
        }
        // The high half is at most `n`, because the magic number is below
        // 2^64: `n - high` does not wrap and `(n - high) / 2 + high` does not
        // overflow.
        let high = ((n as u128 * self.magic as u128) >> 64) as u64;
        if self.add {
            ((n.wrapping_sub(high) >> 1).wrapping_add(high)) >> self.shift
        } else {
            high >> self.shift
        }
    }

    /// [`counter_nanos`]`(ticks, hz)`, exactly, by the same split at whole
    /// seconds with each division a `quotient`, by the reciprocal. A rate past
    /// 18.4 GHz, whose remainder's product no longer fits, takes
    /// [`counter_nanos`] itself.
    #[must_use]
    pub const fn nanos(&self, ticks: u64) -> u64 {
        const NANOS: u64 = 1_000_000_000;
        if self.hz == 0 {
            return 0;
        }
        let seconds = self.quotient(ticks);
        // At most `ticks`, since `seconds` is its floor over `hz`.
        let rest = ticks.wrapping_sub(seconds.wrapping_mul(self.hz));
        let Some(within) = rest.checked_mul(NANOS) else {
            return counter_nanos(ticks, self.hz);
        };
        seconds
            .saturating_mul(NANOS)
            .saturating_add(self.quotient(within))
    }
}

/// The name the image gives itself, Linux's for its vDSO.
pub const SONAME: &str = "linux-vdso.so.1";

/// The name of the return trampoline a signal handler without
/// `SA_RESTORER` goes back through, as Linux's AArch64 vDSO exports it.
pub const SIGRETURN: &str = "__kernel_rt_sigreturn";

/// The version its functions are defined at, which glibc and musl ask for.
pub const VERSION: &str = "LINUX_2.6";

/// `EM_X86_64`.
pub const EM_X86_64: u16 = 62;

/// One function the image exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function<'a> {
    /// Its name, as a C library asks for it: `__vdso_clock_gettime`.
    pub name: &'a str,
    /// The weak alias Linux's vDSO also exports it under, if any:
    /// `clock_gettime`.
    pub alias: Option<&'a str>,
    /// Where it starts, in bytes from the start of the code.
    pub offset: usize,
}

/// What an image is built from.
#[derive(Debug, Clone, Copy)]
pub struct Spec<'a> {
    /// `e_machine`.
    pub machine: u16,
    /// The machine code, placed at [`CODE_AT`].
    pub code: &'a [u8],
    /// What it exports.
    pub functions: &'a [Function<'a>],
}

/// Why an image could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// The page given is not [`IMAGE_BYTES`] long.
    WrongSize,
    /// The code does not fit between [`CODE_AT`] and the end of the page.
    CodeTooLarge,
    /// The headers and tables do not fit below [`CODE_AT`].
    TablesTooLarge,
    /// More functions than the tables are sized for.
    TooManyFunctions,
    /// A function starts outside the code.
    BadOffset,
}

/// The most functions an image exports.
pub const MAX_FUNCTIONS: usize = 8;

/// Symbols in the table at most: the null one, and each function with its
/// alias.
const MAX_SYMBOLS: usize = 1 + 2 * MAX_FUNCTIONS;

/// Bytes of string table the names may take.
const MAX_STRINGS: usize = 512;

// ELF constants, as the specification numbers them.
const ELF_HEADER: usize = 64;
const PROGRAM_HEADER: usize = 56;
const SECTION_HEADER: usize = 64;
const SYMBOL: usize = 24;
const DYNAMIC_ENTRY: usize = 16;
const VERDEF: usize = 20;
const VERDAUX: usize = 8;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PF_W: u32 = 2;
const PF_X: u32 = 1;
const PF_R: u32 = 4;
const ET_DYN: u16 = 3;
const SHT_PROGBITS: u32 = 1;
const SHT_STRTAB: u32 = 3;
const SHT_HASH: u32 = 5;
const SHT_DYNAMIC: u32 = 6;
const SHT_DYNSYM: u32 = 11;
const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;
const SHT_GNU_VERSYM: u32 = 0x6fff_ffff;
const SHF_ALLOC: u64 = 2;
const SHF_EXECINSTR: u64 = 4;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STT_FUNC: u8 = 2;
const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_SONAME: u64 = 14;
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_VERDEFNUM: u64 = 0x6fff_fffd;
const VER_FLG_BASE: u16 = 1;

/// The sections, in the order their headers go: index 0 is the null one.
const SECTIONS: [&str; 9] = [
    "",
    ".hash",
    ".dynsym",
    ".dynstr",
    ".gnu.version",
    ".gnu.version_d",
    ".dynamic",
    ".text",
    ".shstrtab",
];

/// `.text`'s index, which every symbol names as its section: a symbol whose
/// section is undefined is one glibc's lookup passes over.
const TEXT_SECTION: u16 = 7;

/// The System V ELF hash of `name`, as `DT_HASH` buckets and version definitions
/// use it.
#[must_use]
pub fn elf_hash(name: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &byte in name {
        hash = (hash << 4).wrapping_add(u32::from(byte));
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

/// A page being written, every store bounds-checked.
struct Page<'a> {
    bytes: &'a mut [u8],
}

impl Page<'_> {
    fn put(&mut self, at: usize, value: &[u8]) -> Result<(), BuildError> {
        self.bytes
            .get_mut(
                at..at
                    .checked_add(value.len())
                    .ok_or(BuildError::TablesTooLarge)?,
            )
            .ok_or(BuildError::TablesTooLarge)?
            .copy_from_slice(value);
        Ok(())
    }

    fn u16(&mut self, at: usize, value: u16) -> Result<(), BuildError> {
        self.put(at, &value.to_le_bytes())
    }

    fn u32(&mut self, at: usize, value: u32) -> Result<(), BuildError> {
        self.put(at, &value.to_le_bytes())
    }

    fn u64(&mut self, at: usize, value: u64) -> Result<(), BuildError> {
        self.put(at, &value.to_le_bytes())
    }
}

/// A string table being gathered, each string ended by a NUL.
struct Strings {
    bytes: [u8; MAX_STRINGS],
    len: usize,
}

impl Strings {
    fn new() -> Strings {
        // The first byte is the empty string every table starts with.
        Strings {
            bytes: [0; MAX_STRINGS],
            len: 1,
        }
    }

    /// Add `name` and say where it starts.
    fn add(&mut self, name: &str) -> Result<u32, BuildError> {
        let at = self.len;
        let end = at
            .checked_add(name.len())
            .and_then(|end| end.checked_add(1))
            .filter(|&end| end <= MAX_STRINGS)
            .ok_or(BuildError::TablesTooLarge)?;
        self.bytes
            .get_mut(at..end - 1)
            .ok_or(BuildError::TablesTooLarge)?
            .copy_from_slice(name.as_bytes());
        self.len = end;
        u32::try_from(at).map_err(|_| BuildError::TablesTooLarge)
    }

    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

/// One entry of the symbol table being gathered.
#[derive(Clone, Copy, Default)]
struct Symbol {
    name: u32,
    info: u8,
    value: u64,
    hash: u32,
}

/// Round `at` up to a multiple of `to`, a power of two.
const fn align(at: usize, to: usize) -> usize {
    (at + to - 1) & !(to - 1)
}

/// Where each part of the image goes, in bytes from its start, which are
/// also its virtual addresses: the image is linked at zero.
#[derive(Debug, Clone, Copy)]
struct Layout {
    hash: usize,
    dynsym: usize,
    dynstr: usize,
    versym: usize,
    verdef: usize,
    dynamic: usize,
    shstrtab: usize,
    sections: usize,
    end: usize,
}

/// Program headers: the load, the dynamic section and the stack's.
const PROGRAM_HEADERS: usize = 3;

/// Dynamic entries the image has, the terminating null included.
const DYNAMIC_ENTRIES: usize = 10;

impl Layout {
    fn of(symbols: usize, strings: usize, section_names: usize) -> Layout {
        let hash = align(ELF_HEADER + PROGRAM_HEADERS * PROGRAM_HEADER, 8);
        // nbucket, nchain, a bucket and a chain entry per symbol.
        let dynsym = align(hash + 4 * (2 + 2 * symbols), 8);
        let dynstr = dynsym + SYMBOL * symbols;
        let versym = align(dynstr + strings, 2);
        let verdef = align(versym + 2 * symbols, 8);
        let dynamic = align(verdef + 2 * (VERDEF + VERDAUX), 8);
        let shstrtab = dynamic + DYNAMIC_ENTRY * DYNAMIC_ENTRIES;
        let sections = align(shstrtab + section_names, 8);
        let end = sections + SECTION_HEADER * SECTIONS.len();
        Layout {
            hash,
            dynsym,
            dynstr,
            versym,
            verdef,
            dynamic,
            shstrtab,
            sections,
            end,
        }
    }
}

/// Lay out the image `spec` describes in `out`, which must be
/// [`IMAGE_BYTES`] long and is overwritten whole.
///
/// # Errors
///
/// [`BuildError`]; `out` may be partly written.
pub fn build(spec: &Spec<'_>, out: &mut [u8]) -> Result<(), BuildError> {
    if out.len() != IMAGE_BYTES {
        return Err(BuildError::WrongSize);
    }
    if spec.functions.len() > MAX_FUNCTIONS {
        return Err(BuildError::TooManyFunctions);
    }
    let code_end = CODE_AT
        .checked_add(spec.code.len())
        .filter(|&end| end <= IMAGE_BYTES)
        .ok_or(BuildError::CodeTooLarge)?;
    if spec
        .functions
        .iter()
        .any(|function| function.offset >= spec.code.len())
    {
        return Err(BuildError::BadOffset);
    }
    out.fill(0);

    let mut strings = Strings::new();
    let mut symbols = [Symbol::default(); MAX_SYMBOLS];
    let mut count = 1;
    for function in spec.functions {
        let value = (CODE_AT + function.offset) as u64;
        let names = [
            Some((function.name, STB_GLOBAL)),
            function.alias.map(|alias| (alias, STB_WEAK)),
        ];
        for (name, bind) in names.into_iter().flatten() {
            let slot = symbols.get_mut(count).ok_or(BuildError::TooManyFunctions)?;
            *slot = Symbol {
                name: strings.add(name)?,
                info: (bind << 4) | STT_FUNC,
                value,
                hash: elf_hash(name.as_bytes()),
            };
            count += 1;
        }
    }
    let symbols = symbols.get(..count).ok_or(BuildError::TooManyFunctions)?;
    let soname = strings.add(SONAME)?;
    let version = strings.add(VERSION)?;

    let mut section_names = Strings::new();
    let mut section_name = [0_u32; SECTIONS.len()];
    for (slot, name) in section_name.iter_mut().zip(SECTIONS).skip(1) {
        *slot = section_names.add(name)?;
    }

    let layout = Layout::of(count, strings.len, section_names.len);
    if layout.end > CODE_AT {
        return Err(BuildError::TablesTooLarge);
    }
    let mut page = Page { bytes: out };
    header(&mut page, spec.machine, &layout)?;
    hash_table(&mut page, &layout, symbols)?;
    for (index, symbol) in symbols.iter().enumerate().skip(1) {
        let at = layout.dynsym + SYMBOL * index;
        page.u32(at, symbol.name)?;
        page.put(at + 4, &[symbol.info, 0])?;
        page.u16(at + 6, TEXT_SECTION)?;
        page.u64(at + 8, symbol.value)?;
    }
    page.put(layout.dynstr, strings.as_bytes())?;
    // Every exported symbol at version 2, `LINUX_2.6`; the null one local.
    for index in 1..count {
        page.u16(layout.versym + 2 * index, 2)?;
    }
    versions(&mut page, &layout, soname, version)?;
    dynamic(&mut page, &layout, strings.len, soname)?;
    page.put(layout.shstrtab, section_names.as_bytes())?;
    let tables = Tables {
        layout,
        symbols: count,
        strings: strings.len,
        section_names: section_names.len,
        code_len: spec.code.len(),
    };
    section_headers(&mut page, &tables, &section_name)?;
    page.put(CODE_AT, spec.code)?;
    debug_assert!(code_end <= IMAGE_BYTES, "the code was checked to fit");
    Ok(())
}

/// The ELF header and the program headers.
fn header(page: &mut Page<'_>, machine: u16, layout: &Layout) -> Result<(), BuildError> {
    // Magic, 64-bit, little-endian, version 1, the System V ABI.
    page.put(0, &[0x7f, b'E', b'L', b'F', 2, 1, 1, 0])?;
    page.u16(16, ET_DYN)?;
    page.u16(18, machine)?;
    page.u32(20, 1)?;
    page.u64(24, 0)?;
    page.u64(32, ELF_HEADER as u64)?;
    page.u64(40, layout.sections as u64)?;
    page.u32(48, 0)?;
    page.u16(52, ELF_HEADER as u16)?;
    page.u16(54, PROGRAM_HEADER as u16)?;
    page.u16(56, PROGRAM_HEADERS as u16)?;
    page.u16(58, SECTION_HEADER as u16)?;
    page.u16(60, SECTIONS.len() as u16)?;
    page.u16(62, (SECTIONS.len() - 1) as u16)?;

    // The whole page, read and executed, linked at zero.
    let load = ELF_HEADER;
    page.u32(load, PT_LOAD)?;
    page.u32(load + 4, PF_R | PF_X)?;
    page.u64(load + 8, 0)?;
    page.u64(load + 16, 0)?;
    page.u64(load + 24, 0)?;
    page.u64(load + 32, IMAGE_BYTES as u64)?;
    page.u64(load + 40, IMAGE_BYTES as u64)?;
    page.u64(load + 48, IMAGE_BYTES as u64)?;

    // The dynamic section, read-only, which is how glibc knows not to
    // relocate its entries in place.
    let dynamic = ELF_HEADER + PROGRAM_HEADER;
    let size = (DYNAMIC_ENTRY * DYNAMIC_ENTRIES) as u64;
    page.u32(dynamic, PT_DYNAMIC)?;
    page.u32(dynamic + 4, PF_R)?;
    page.u64(dynamic + 8, layout.dynamic as u64)?;
    page.u64(dynamic + 16, layout.dynamic as u64)?;
    page.u64(dynamic + 24, layout.dynamic as u64)?;
    page.u64(dynamic + 32, size)?;
    page.u64(dynamic + 40, size)?;
    page.u64(dynamic + 48, 8)?;

    // A stack that need not be executable, which Linux's vDSO says too, and
    // without which a loader that maps the image as a library -- the host's
    // `dlopen`, in a cross-check -- takes it to need one.
    let stack = ELF_HEADER + 2 * PROGRAM_HEADER;
    page.u32(stack, PT_GNU_STACK)?;
    page.u32(stack + 4, PF_R | PF_W)?;
    page.u64(stack + 48, 16)
}

/// `DT_HASH`: as many buckets as symbols, each the head of a chain.
fn hash_table(page: &mut Page<'_>, layout: &Layout, symbols: &[Symbol]) -> Result<(), BuildError> {
    let count = symbols.len();
    let buckets = layout.hash + 8;
    let chains = buckets + 4 * count;
    page.u32(layout.hash, count as u32)?;
    page.u32(layout.hash + 4, count as u32)?;
    // Each symbol goes on the front of its bucket's chain, so a bucket names
    // its last symbol and each chain entry the one added before.
    let mut heads = [0_u32; MAX_SYMBOLS];
    for (index, symbol) in symbols.iter().enumerate().skip(1) {
        let bucket = symbol.hash as usize % count;
        let head = heads.get_mut(bucket).ok_or(BuildError::TablesTooLarge)?;
        page.u32(chains + 4 * index, *head)?;
        *head = index as u32;
    }
    for (bucket, head) in heads.iter().take(count).enumerate() {
        page.u32(buckets + 4 * bucket, *head)?;
    }
    Ok(())
}

/// `DT_VERDEF`: the base version, which is the image's own name, and
/// `LINUX_2.6`.
fn versions(
    page: &mut Page<'_>,
    layout: &Layout,
    soname: u32,
    version: u32,
) -> Result<(), BuildError> {
    let definitions = [
        (VER_FLG_BASE, 1_u16, SONAME, soname),
        (0, 2_u16, VERSION, version),
    ];
    let mut at = layout.verdef;
    for (index, (flags, ndx, name, offset)) in definitions.into_iter().enumerate() {
        let last = index + 1 == definitions.len();
        page.u16(at, 1)?;
        page.u16(at + 2, flags)?;
        page.u16(at + 4, ndx)?;
        page.u16(at + 6, 1)?;
        page.u32(at + 8, elf_hash(name.as_bytes()))?;
        page.u32(at + 12, VERDEF as u32)?;
        page.u32(at + 16, if last { 0 } else { (VERDEF + VERDAUX) as u32 })?;
        page.u32(at + VERDEF, offset)?;
        page.u32(at + VERDEF + 4, 0)?;
        at += VERDEF + VERDAUX;
    }
    Ok(())
}

/// The dynamic section.
fn dynamic(
    page: &mut Page<'_>,
    layout: &Layout,
    strings: usize,
    soname: u32,
) -> Result<(), BuildError> {
    let entries: [(u64, u64); DYNAMIC_ENTRIES] = [
        (DT_HASH, layout.hash as u64),
        (DT_STRTAB, layout.dynstr as u64),
        (DT_SYMTAB, layout.dynsym as u64),
        (DT_STRSZ, strings as u64),
        (DT_SYMENT, SYMBOL as u64),
        (DT_SONAME, u64::from(soname)),
        (DT_VERSYM, layout.versym as u64),
        (DT_VERDEF, layout.verdef as u64),
        (DT_VERDEFNUM, 2),
        (DT_NULL, 0),
    ];
    for (index, (tag, value)) in entries.into_iter().enumerate() {
        page.u64(layout.dynamic + DYNAMIC_ENTRY * index, tag)?;
        page.u64(layout.dynamic + DYNAMIC_ENTRY * index + 8, value)?;
    }
    Ok(())
}

/// What the section headers describe.
struct Tables {
    layout: Layout,
    symbols: usize,
    strings: usize,
    section_names: usize,
    code_len: usize,
}

/// One section header's fields, beside its name.
#[derive(Clone, Copy)]
struct Row {
    kind: u32,
    flags: u64,
    start: usize,
    size: usize,
    link: u32,
    info: u32,
    alignment: u64,
    entry: u64,
}

/// The section headers: not needed to load the image or look anything up
/// in it, but what `readelf`, a debugger and a crash reporter read.
fn section_headers(page: &mut Page<'_>, tables: &Tables, names: &[u32]) -> Result<(), BuildError> {
    let layout = &tables.layout;
    let row = |kind, start, size, link, info, alignment, entry| Row {
        kind,
        flags: SHF_ALLOC,
        start,
        size,
        link,
        info,
        alignment,
        entry,
    };
    let symbols = tables.symbols;
    let rows = [
        row(SHT_HASH, layout.hash, 4 * (2 + 2 * symbols), 2, 0, 8, 4),
        row(
            SHT_DYNSYM,
            layout.dynsym,
            SYMBOL * symbols,
            3,
            1,
            8,
            SYMBOL as u64,
        ),
        row(SHT_STRTAB, layout.dynstr, tables.strings, 0, 0, 1, 0),
        row(SHT_GNU_VERSYM, layout.versym, 2 * symbols, 2, 0, 2, 2),
        row(
            SHT_GNU_VERDEF,
            layout.verdef,
            2 * (VERDEF + VERDAUX),
            3,
            2,
            8,
            0,
        ),
        row(
            SHT_DYNAMIC,
            layout.dynamic,
            DYNAMIC_ENTRY * DYNAMIC_ENTRIES,
            3,
            0,
            8,
            DYNAMIC_ENTRY as u64,
        ),
        Row {
            flags: SHF_ALLOC | SHF_EXECINSTR,
            ..row(SHT_PROGBITS, CODE_AT, tables.code_len, 0, 0, 16, 0)
        },
        Row {
            flags: 0,
            ..row(
                SHT_STRTAB,
                layout.shstrtab,
                tables.section_names,
                0,
                0,
                1,
                0,
            )
        },
    ];
    for (index, row) in rows.into_iter().enumerate() {
        let at = layout.sections + SECTION_HEADER * (index + 1);
        let name = names
            .get(index + 1)
            .copied()
            .ok_or(BuildError::TablesTooLarge)?;
        // `.shstrtab` is not loaded, so it has an offset and no address.
        let address = if row.flags & SHF_ALLOC == 0 {
            0
        } else {
            row.start as u64
        };
        page.u32(at, name)?;
        page.u32(at + 4, row.kind)?;
        page.u64(at + 8, row.flags)?;
        page.u64(at + 16, address)?;
        page.u64(at + 24, row.start as u64)?;
        page.u64(at + 32, row.size as u64)?;
        page.u32(at + 40, row.link)?;
        page.u32(at + 44, row.info)?;
        page.u64(at + 48, row.alignment)?;
        page.u64(at + 56, row.entry)?;
    }
    Ok(())
}

/// Read a little-endian word of `N` bytes at `at`.
fn read<const N: usize>(image: &[u8], at: usize) -> Option<[u8; N]> {
    image.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn read16(image: &[u8], at: usize) -> Option<u16> {
    read::<2>(image, at).map(u16::from_le_bytes)
}

fn read32(image: &[u8], at: usize) -> Option<u32> {
    read::<4>(image, at).map(u32::from_le_bytes)
}

fn read64(image: &[u8], at: usize) -> Option<usize> {
    usize::try_from(read::<8>(image, at).map(u64::from_le_bytes)?).ok()
}

/// The NUL-terminated string at `at`.
fn string(image: &[u8], at: usize) -> Option<&[u8]> {
    let rest = image.get(at..)?;
    rest.get(..rest.iter().position(|&byte| byte == 0)?)
}

/// What [`lookup`] reads out of an image's dynamic section.
struct Dynamic {
    strtab: usize,
    symtab: usize,
    hash: usize,
    versym: Option<usize>,
    verdef: Option<usize>,
}

/// Read the dynamic section of `image`, which is loaded at its own start, as
/// musl's `__vdsosym` reads it: through the program headers, not the
/// sections.
fn dynamic_of(image: &[u8]) -> Option<Dynamic> {
    if image.get(..6)? != [0x7f, b'E', b'L', b'F', 2, 1] {
        return None;
    }
    let phoff = read64(image, 32)?;
    let phnum = usize::from(read16(image, 56)?);
    let mut base = None;
    let mut dynamic = None;
    for index in 0..phnum {
        let at = phoff.checked_add(PROGRAM_HEADER.checked_mul(index)?)?;
        match read32(image, at)? {
            PT_LOAD if base.is_none() => {
                // Where the segment's first byte is, less its address: what
                // an address in the image is relative to.
                base = Some(read64(image, at + 8)?.checked_sub(read64(image, at + 16)?)?);
            }
            PT_DYNAMIC => dynamic = Some(read64(image, at + 16)?),
            _ => {}
        }
    }
    let (base, dynamic) = (base?, dynamic?);
    let mut found = Dynamic {
        strtab: 0,
        symtab: 0,
        hash: 0,
        versym: None,
        verdef: None,
    };
    let mut at = base.checked_add(dynamic)?;
    loop {
        let tag = read64(image, at)? as u64;
        let value = base.checked_add(read64(image, at + 8)?)?;
        match tag {
            DT_NULL => break,
            DT_STRTAB => found.strtab = value,
            DT_SYMTAB => found.symtab = value,
            DT_HASH => found.hash = value,
            DT_VERSYM => found.versym = Some(value),
            DT_VERDEF => found.verdef = Some(value),
            _ => {}
        }
        at = at.checked_add(DYNAMIC_ENTRY)?;
    }
    (found.strtab != 0 && found.symtab != 0 && found.hash != 0).then_some(found)
}

/// Whether version index `ndx` is named `version` in the definitions at
/// `verdef`, as musl's `checkver` asks.
fn version_is(image: &[u8], dynamic: &Dynamic, ndx: u16, version: &[u8]) -> Option<bool> {
    let mut at = dynamic.verdef?;
    loop {
        if read16(image, at + 2)? & VER_FLG_BASE == 0
            && read16(image, at + 4)? & 0x7fff == ndx & 0x7fff
        {
            let aux = at.checked_add(usize::try_from(read32(image, at + 12)?).ok()?)?;
            let name = usize::try_from(read32(image, aux)?).ok()?;
            return Some(string(image, dynamic.strtab.checked_add(name)?)? == version);
        }
        let next = usize::try_from(read32(image, at + 16)?).ok()?;
        if next == 0 {
            return Some(false);
        }
        at = at.checked_add(next)?;
    }
}

/// Whether symbol `index` is a defined, exported function or object called
/// `name`, at `version` when one is asked for.
fn matches(
    image: &[u8],
    dynamic: &Dynamic,
    index: usize,
    name: &[u8],
    version: Option<&[u8]>,
) -> Option<bool> {
    let at = dynamic.symtab.checked_add(SYMBOL.checked_mul(index)?)?;
    let info = *image.get(at + 4)?;
    let kind = info & 0xf;
    let bind = info >> 4;
    // `STT_NOTYPE`, `STT_OBJECT`, `STT_FUNC`; global or weak; defined.
    if kind > STT_FUNC || !(bind == STB_GLOBAL || bind == STB_WEAK) || read16(image, at + 6)? == 0 {
        return Some(false);
    }
    let offset = usize::try_from(read32(image, at)?).ok()?;
    if string(image, dynamic.strtab.checked_add(offset)?)? != name {
        return Some(false);
    }
    match (version, dynamic.versym) {
        (Some(version), Some(versym)) => version_is(
            image,
            dynamic,
            read16(image, versym.checked_add(2 * index)?)?,
            version,
        ),
        _ => Some(true),
    }
}

/// Where symbol `name` is, in bytes from the image's start, found by walking
/// the whole table as musl does; at `version` when one is given.
#[must_use]
pub fn lookup(image: &[u8], name: &str, version: Option<&str>) -> Option<usize> {
    let dynamic = dynamic_of(image)?;
    let count = usize::try_from(read32(image, dynamic.hash + 4)?).ok()?;
    let version = version.map(str::as_bytes);
    for index in 1..count {
        if matches(image, &dynamic, index, name.as_bytes(), version)? {
            return read64(image, dynamic.symtab + SYMBOL * index + 8);
        }
    }
    None
}

/// [`lookup`], found through the hash table's buckets and chains as glibc's
/// `do_lookup_x` finds it, so that a table whose chains do not reach every
/// symbol fails a test.
#[must_use]
pub fn lookup_hashed(image: &[u8], name: &str, version: Option<&str>) -> Option<usize> {
    let dynamic = dynamic_of(image)?;
    let buckets = usize::try_from(read32(image, dynamic.hash)?).ok()?;
    let count = usize::try_from(read32(image, dynamic.hash + 4)?).ok()?;
    let chains = dynamic.hash + 8 + 4 * buckets;
    let bucket = usize::try_from(elf_hash(name.as_bytes())).ok()? % buckets;
    let mut index = usize::try_from(read32(image, dynamic.hash + 8 + 4 * bucket)?).ok()?;
    let version = version.map(str::as_bytes);
    // A chain is at most as long as the table; one longer is a loop.
    for _ in 0..count {
        if index == 0 {
            return None;
        }
        if matches(image, &dynamic, index, name.as_bytes(), version)? {
            return read64(image, dynamic.symtab + SYMBOL * index + 8);
        }
        index = usize::try_from(read32(image, chains + 4 * index)?).ok()?;
    }
    None
}

#[cfg(test)]
mod tests;
