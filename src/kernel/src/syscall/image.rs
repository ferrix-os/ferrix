//! A synthetic ELF image, for the loader's self-check.
//!
//! The loader has to be exercised on the machine, because what it produces is
//! page tables and there is no way to check those from the host. That needs an
//! image, and the alternatives to building one here are worse: a binary
//! committed to the tree would be three binaries, one per architecture, none
//! of them readable in a diff, and all of them stale the moment the loader's
//! assumptions changed.
//!
//! So the check builds its own, for whichever architecture it is running on,
//! with the shape that matters — a read-execute segment covering the headers
//! and a read-write one with more `p_memsz` than `p_filesz`, which is how
//! `.bss` is expressed and the one part of loading with nothing to copy.
//!
//! Deliberately not a general ELF writer. It emits exactly what the check
//! needs and is wrong for anything else: no sections, no dynamic table, no
//! alignment congruence between file offset and virtual address.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};
use ferrix_elf::{Class, PF_R, PF_W, PF_X, PT_INTERP, PT_LOAD};

/// Where the synthetic image is linked. Well above zero and well below
/// anything the kernel uses, on every architecture including the 32-bit one.
pub(crate) const BASE: u64 = 0x0040_0000;

/// The entry point the image declares: a little way into the text segment.
pub(crate) const ENTRY: u64 = BASE + 0x100;

/// Where the writable segment is linked.
pub(crate) const DATA_VADDR: u64 = BASE + 0x0001_0000;

/// Bytes of the data segment that come from the file; the rest is `.bss`.
pub(crate) const DATA_FILESZ: usize = 16;

/// Bytes the data segment occupies in memory.
pub(crate) const DATA_MEMSZ: u64 = PAGE_SIZE + 32;

/// The pattern the data segment's file contents carry, so the check can
/// prove the bytes landed at the address the program headers asked for and
/// not merely somewhere.
pub(crate) const DATA_MARK: [u8; DATA_FILESZ] = *b"stage7-loaded-ok";

/// Where a [`Shape::Dynamic`] image's `PT_INTERP` sits: inside the text
/// segment, as a linked binary's does, and past the entry point's payload.
const INTERP_OFFSET: u64 = 0x300;

/// The path a [`Shape::Dynamic`] image names as its dynamic linker, with the
/// NUL a `PT_INTERP` carries.
pub(crate) const INTERP_PATH: &[u8] = b"/ld-check\0";

/// How the image should be built, for the checks that want a broken one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Valid: read-execute text, read-write data with a `.bss` tail.
    Good,
    /// `ET_DYN` with no interpreter: a static PIE, placed at the loader's
    /// base rather than where it is linked.
    PositionIndependent,
    /// `ET_DYN` naming [`INTERP_PATH`] as its dynamic linker: a program the
    /// kernel may not enter, because something has to resolve what it imports
    /// first.
    Dynamic,
    /// A machine this kernel is not.
    ForeignMachine,
    /// Text and data linked into the same page, so the page would have to be
    /// writable and executable at once.
    WriteExecute,
    /// An entry point at the first address past the user half: non-canonical
    /// on x86-64, the kernel's half on the other two.
    EntryOutsideUser,
}

/// Build one.
pub(crate) fn build(class: Class, machine: u16, shape: Shape) -> Vec<u8> {
    build_with(class, machine, shape, &[])
}

/// Build one carrying `code` at its entry point.
///
/// The payload is what makes the image a *program* rather than a shape: the
/// loader checks use an empty one, and the check that actually enters ring 3
/// passes the architecture's own machine code.
pub fn build_with(class: Class, machine: u16, shape: Shape, code: &[u8]) -> Vec<u8> {
    let machine = match shape {
        // 0x3E is x86-64 and 0xB7 is AArch64; whichever this is not.
        Shape::ForeignMachine if machine == 0x003E => 0x00B7,
        Shape::ForeignMachine => 0x003E,
        _ => machine,
    };
    let data_vaddr = match shape {
        // Land the writable segment inside the executable one's last page.
        Shape::WriteExecute => BASE + 0x200,
        _ => DATA_VADDR,
    };

    let header = class.header_size();
    let phentsize = class.phdr_size();
    let phoff = header;
    // A dynamic image carries a third program header, the `PT_INTERP`.
    let phnum: u16 = if shape == Shape::Dynamic { 3 } else { 2 };
    let text_filesz = phoff + phentsize * usize::from(phnum) + 0x400;
    let data_offset = usize::try_from(PAGE_SIZE).unwrap_or(4096);

    let mut file = vec![0_u8; data_offset + DATA_FILESZ];

    write_header(&mut file, class, machine, shape, phoff, phnum);
    // The text segment covers the file from zero, which is what puts the
    // program headers at a real address and so gives `AT_PHDR` something to
    // point at -- exactly as a linked binary does.
    write_phdr(
        &mut file,
        class,
        phoff,
        &Phdr {
            kind: PT_LOAD,
            flags: PF_R | PF_X,
            offset: 0,
            vaddr: BASE,
            filesz: text_filesz as u64,
            memsz: text_filesz as u64,
        },
    );
    write_phdr(
        &mut file,
        class,
        phoff + phentsize,
        &Phdr {
            kind: PT_LOAD,
            flags: PF_R | PF_W,
            offset: data_offset as u64,
            vaddr: data_vaddr,
            filesz: DATA_FILESZ as u64,
            memsz: DATA_MEMSZ,
        },
    );
    if let Some(slot) = file.get_mut(data_offset..data_offset + DATA_FILESZ) {
        slot.copy_from_slice(&DATA_MARK);
    }

    // The `PT_INTERP`: a window into the read-execute segment, which is where
    // a linked binary carries its interpreter's name too.
    if shape == Shape::Dynamic {
        let at = usize::try_from(INTERP_OFFSET).unwrap_or(0);
        if let Some(slot) = file.get_mut(at..at + INTERP_PATH.len()) {
            slot.copy_from_slice(INTERP_PATH);
        }
        write_phdr(
            &mut file,
            class,
            phoff + phentsize * 2,
            &Phdr {
                kind: PT_INTERP,
                flags: PF_R,
                offset: INTERP_OFFSET,
                vaddr: BASE + INTERP_OFFSET,
                filesz: INTERP_PATH.len() as u64,
                memsz: INTERP_PATH.len() as u64,
            },
        );
    }

    // The entry point is `ENTRY - BASE` into the text segment, and that
    // segment starts at file offset zero, so the payload goes at the same
    // offset in the file.
    let at = usize::try_from(ENTRY - BASE).unwrap_or(0);
    if let Some(slot) = file.get_mut(at..at + code.len()) {
        slot.copy_from_slice(code);
    }
    file
}

/// A third loadable segment, for [`build_with_segment`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Extra {
    /// `PF_*`.
    pub(crate) flags: u32,
    /// Where its contents are in the file.
    pub(crate) offset: u64,
    /// Where it is linked.
    pub(crate) vaddr: u64,
    /// Bytes of it in the file.
    pub(crate) filesz: u64,
    /// Bytes of it in memory.
    pub(crate) memsz: u64,
}

/// A [`Shape::Good`] image carrying `code`, with a third loadable segment
/// `extra` whose contents are not in the bytes returned.
///
/// The headers of a file larger than anything worth building in memory: the
/// check that loads it supplies the rest of the file from elsewhere. The
/// third program header sits where the text segment already covers it, after
/// the two a good image has and before its entry point.
pub(crate) fn build_with_segment(
    class: Class,
    machine: u16,
    code: &[u8],
    extra: &Extra,
) -> Vec<u8> {
    let mut file = build_with(class, machine, Shape::Good, code);
    let phnum_at = match class {
        Class::Elf32 => 44,
        Class::Elf64 => 56,
    };
    put(&mut file, phnum_at, &3_u16.to_le_bytes());
    write_phdr(
        &mut file,
        class,
        class.header_size() + class.phdr_size() * 2,
        &Phdr {
            kind: PT_LOAD,
            flags: extra.flags,
            offset: extra.offset,
            vaddr: extra.vaddr,
            filesz: extra.filesz,
            memsz: extra.memsz,
        },
    );
    file
}

/// One program header, before it is written out.
struct Phdr {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

/// `e_ident` and the fields after it, at the offsets the class puts them.
fn write_header(
    file: &mut [u8],
    class: Class,
    machine: u16,
    shape: Shape,
    phoff: usize,
    phnum: u16,
) {
    put(file, 0, &[0x7F, b'E', b'L', b'F']);
    let (class_byte, header_size, phentsize) = match class {
        Class::Elf32 => (1_u8, 52_u16, 32_u16),
        Class::Elf64 => (2, 64, 56),
    };
    put(file, 4, &[class_byte, 1, 1, 0]);

    // ET_DYN is 3 and ET_EXEC is 2. The loader moves the former to its base.
    let elf_type: u16 = if matches!(shape, Shape::PositionIndependent | Shape::Dynamic) {
        3
    } else {
        2
    };
    put(file, 16, &elf_type.to_le_bytes());
    put(file, 18, &machine.to_le_bytes());
    put(file, 20, &1_u32.to_le_bytes());

    let entry = if shape == Shape::EntryOutsideUser {
        USER_VIRT_END
    } else {
        ENTRY
    };
    match class {
        Class::Elf32 => {
            put(file, 24, &(entry as u32).to_le_bytes());
            put(file, 28, &(phoff as u32).to_le_bytes());
            put(file, 40, &header_size.to_le_bytes());
            put(file, 42, &phentsize.to_le_bytes());
            put(file, 44, &phnum.to_le_bytes());
        }
        Class::Elf64 => {
            put(file, 24, &entry.to_le_bytes());
            put(file, 32, &(phoff as u64).to_le_bytes());
            put(file, 52, &header_size.to_le_bytes());
            put(file, 54, &phentsize.to_le_bytes());
            put(file, 56, &phnum.to_le_bytes());
        }
    }
}

/// One program header. The two classes reorder the fields as well as widening
/// them, which is the thing this and `src/lib/platform/elf` have to agree about.
fn write_phdr(file: &mut [u8], class: Class, at: usize, phdr: &Phdr) {
    match class {
        Class::Elf32 => {
            put(file, at, &phdr.kind.to_le_bytes());
            put(file, at + 4, &(phdr.offset as u32).to_le_bytes());
            put(file, at + 8, &(phdr.vaddr as u32).to_le_bytes());
            put(file, at + 12, &(phdr.vaddr as u32).to_le_bytes());
            put(file, at + 16, &(phdr.filesz as u32).to_le_bytes());
            put(file, at + 20, &(phdr.memsz as u32).to_le_bytes());
            put(file, at + 24, &phdr.flags.to_le_bytes());
            put(file, at + 28, &(PAGE_SIZE as u32).to_le_bytes());
        }
        Class::Elf64 => {
            put(file, at, &phdr.kind.to_le_bytes());
            put(file, at + 4, &phdr.flags.to_le_bytes());
            put(file, at + 8, &phdr.offset.to_le_bytes());
            put(file, at + 16, &phdr.vaddr.to_le_bytes());
            put(file, at + 24, &phdr.vaddr.to_le_bytes());
            put(file, at + 32, &phdr.filesz.to_le_bytes());
            put(file, at + 40, &phdr.memsz.to_le_bytes());
            put(file, at + 48, &PAGE_SIZE.to_le_bytes());
        }
    }
}

/// Write bytes at an offset, doing nothing if they would not fit.
///
/// Silent rather than panicking: the buffer is sized by this module and a
/// short write would fail the check that reads the image back, which is a
/// better place to notice than a panic in a helper.
fn put(file: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(slot) = file.get_mut(at..at + bytes.len()) {
        slot.copy_from_slice(bytes);
    }
}
