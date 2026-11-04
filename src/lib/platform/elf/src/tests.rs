//! Tests for the ELF reader.
//!
//! Images are built by hand rather than checked in as binaries, so that a test
//! failure names the field that is wrong instead of pointing at an opaque blob,
//! and so that the malformed cases can be produced by mutating one field of a
//! known-good image. The same shapes seed `src/tests/fuzz/fuzz_targets/elf_parse.rs`, by
//! way of `tools/common/gen/seed-fuzz-corpus.py`.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

/// Where a segment's file contents come from.
enum Contents {
    /// Bytes of its own, appended to the image.
    Owned(Vec<u8>),
    /// A window into an earlier segment's bytes: which segment, and how far
    /// in. This is how a `PT_DYNAMIC` sits inside the `PT_LOAD` that maps it.
    Within(usize, u64),
}

/// A synthetic ELF image under construction.
pub(crate) struct Builder {
    class: Class,
    elf_type: u16,
    machine: u16,
    entry: u64,
    segments: Vec<Segment>,
    contents: Vec<Contents>,
}

impl Builder {
    /// A 64-bit x86-64 executable.
    fn new() -> Self {
        Builder {
            class: Class::Elf64,
            elf_type: ET_EXEC,
            machine: EM_X86_64,
            entry: 0xFFFF_FFFF_8000_1000,
            segments: Vec::new(),
            contents: Vec::new(),
        }
    }

    /// A 32-bit Arm executable, which is what the ARMv7-A kernel is.
    fn arm32() -> Self {
        Builder {
            class: Class::Elf32,
            machine: EM_ARM,
            entry: 0xF000_0000,
            ..Builder::new()
        }
    }

    fn machine(mut self, machine: u16) -> Self {
        self.machine = machine;
        self
    }

    fn elf_type(mut self, elf_type: u16) -> Self {
        self.elf_type = elf_type;
        self
    }

    /// Add a segment whose file contents are `data`, followed by
    /// `zero_fill` bytes of `.bss`.
    fn segment(mut self, kind: u32, flags: u32, vaddr: u64, data: Vec<u8>, zero_fill: u64) -> Self {
        self.segments.push(Segment {
            kind,
            flags,
            offset: 0, // patched in `build`
            vaddr,
            filesz: data.len() as u64,
            memsz: data.len() as u64 + zero_fill,
            align: 0x1000,
        });
        self.contents.push(Contents::Owned(data));
        self
    }

    /// Add a segment of `len` bytes whose contents are the bytes `delta` into
    /// segment `index`.
    fn within(mut self, kind: u32, vaddr: u64, index: usize, delta: u64, len: u64) -> Self {
        self.segments.push(Segment {
            kind,
            flags: PF_R,
            offset: 0,
            vaddr,
            filesz: len,
            memsz: len,
            align: 8,
        });
        self.contents.push(Contents::Within(index, delta));
        self
    }

    fn build(self) -> Vec<u8> {
        let class = self.class;
        let phnum = self.segments.len();
        let phoff = class.header_size();
        let mut image = vec![0u8; phoff + phnum * class.phdr_size()];

        image[0..4].copy_from_slice(&ELF_MAGIC);
        image[4] = match class {
            Class::Elf32 => ELFCLASS32,
            Class::Elf64 => ELFCLASS64,
        };
        image[5] = ELFDATA2LSB;
        image[6] = 1; // EI_VERSION
        image[16..18].copy_from_slice(&self.elf_type.to_le_bytes());
        image[18..20].copy_from_slice(&self.machine.to_le_bytes());
        image[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        match class {
            Class::Elf64 => {
                image[24..32].copy_from_slice(&self.entry.to_le_bytes());
                image[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
                image[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
                image[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
                image[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
            }
            Class::Elf32 => {
                image[24..28].copy_from_slice(&(self.entry as u32).to_le_bytes());
                image[28..32].copy_from_slice(&(phoff as u32).to_le_bytes());
                // EABI version 5, soft-float: what rustc writes for both Arm
                // targets this project builds for.
                image[36..40].copy_from_slice(&0x0500_0200u32.to_le_bytes());
                image[40..42].copy_from_slice(&(EHDR32_SIZE as u16).to_le_bytes());
                image[42..44].copy_from_slice(&(PHDR32_SIZE as u16).to_le_bytes());
                image[44..46].copy_from_slice(&(phnum as u16).to_le_bytes());
            }
        }

        let mut offsets = Vec::new();
        for contents in &self.contents {
            let offset = match contents {
                Contents::Owned(data) => {
                    let offset = image.len() as u64;
                    image.extend_from_slice(data);
                    offset
                }
                Contents::Within(index, delta) => offsets[*index] + delta,
            };
            offsets.push(offset);
        }

        for (index, (mut segment, offset)) in self.segments.into_iter().zip(offsets).enumerate() {
            segment.offset = offset;
            let base = phoff + index * class.phdr_size();
            write_phdr(class, &mut image[base..base + class.phdr_size()], &segment);
        }

        image
    }
}

/// Lay one program header out in `slot`, in `class`'s field order.
fn write_phdr(class: Class, slot: &mut [u8], segment: &Segment) {
    match class {
        Class::Elf64 => {
            slot[0..4].copy_from_slice(&segment.kind.to_le_bytes());
            slot[4..8].copy_from_slice(&segment.flags.to_le_bytes());
            slot[8..16].copy_from_slice(&segment.offset.to_le_bytes());
            slot[16..24].copy_from_slice(&segment.vaddr.to_le_bytes());
            slot[24..32].copy_from_slice(&segment.vaddr.to_le_bytes()); // p_paddr
            slot[32..40].copy_from_slice(&segment.filesz.to_le_bytes());
            slot[40..48].copy_from_slice(&segment.memsz.to_le_bytes());
            slot[48..56].copy_from_slice(&segment.align.to_le_bytes());
        }
        Class::Elf32 => {
            slot[0..4].copy_from_slice(&segment.kind.to_le_bytes());
            slot[4..8].copy_from_slice(&(segment.offset as u32).to_le_bytes());
            slot[8..12].copy_from_slice(&(segment.vaddr as u32).to_le_bytes());
            slot[12..16].copy_from_slice(&(segment.vaddr as u32).to_le_bytes()); // p_paddr
            slot[16..20].copy_from_slice(&(segment.filesz as u32).to_le_bytes());
            slot[20..24].copy_from_slice(&(segment.memsz as u32).to_le_bytes());
            slot[24..28].copy_from_slice(&segment.flags.to_le_bytes());
            slot[28..32].copy_from_slice(&(segment.align as u32).to_le_bytes());
        }
    }
}

/// A dynamic section holding `tags`, in `class`'s entry format.
fn dynamic(class: Class, tags: &[(i64, u64)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for &(tag, value) in tags {
        match class {
            Class::Elf64 => {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            Class::Elf32 => {
                bytes.extend_from_slice(&(tag as i32).to_le_bytes());
                bytes.extend_from_slice(&(value as u32).to_le_bytes());
            }
        }
    }
    bytes
}

/// Link address of the one `PT_LOAD` in [`static_pie`].
const PIE_BASE: u64 = 0x1000;

/// A static PIE: one writable `PT_LOAD` holding `table` at [`PIE_BASE`] and a
/// dynamic section of `tags` right behind it, which a `PT_DYNAMIC` names.
fn static_pie(builder: Builder, table: Vec<u8>, tags: &[(i64, u64)]) -> Vec<u8> {
    let class = builder.class;
    let dyn_offset = table.len() as u64;
    let dyn_bytes = dynamic(class, tags);
    let dyn_len = dyn_bytes.len() as u64;

    let mut payload = table;
    payload.extend_from_slice(&dyn_bytes);

    builder
        .elf_type(ET_DYN)
        .segment(PT_LOAD, PF_R | PF_W, PIE_BASE, payload, 0)
        .within(PT_DYNAMIC, PIE_BASE + dyn_offset, 0, dyn_offset, dyn_len)
        .build()
}

/// A two-segment kernel-shaped image: read-execute text, read-write data with
/// a `.bss` tail.
fn kernel_image() -> Vec<u8> {
    Builder::new()
        .segment(
            PT_LOAD,
            PF_R | PF_X,
            0xFFFF_FFFF_8000_0000,
            vec![0x90; 0x1000],
            0,
        )
        .segment(
            PT_LOAD,
            PF_R | PF_W,
            0xFFFF_FFFF_8000_1000,
            vec![0xAA; 0x800],
            0x800,
        )
        .build()
}

/// The same shape, 32-bit, at the address the ARMv7-A kernel is linked for.
fn arm32_kernel_image() -> Vec<u8> {
    Builder::arm32()
        .segment(PT_LOAD, PF_R | PF_X, 0xF000_0000, vec![0xE3; 0x1000], 0)
        .segment(PT_LOAD, PF_R | PF_W, 0xF000_1000, vec![0x55; 0x600], 0xA00)
        .build()
}

#[test]
fn parses_a_well_formed_image() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();

    assert_eq!(elf.class(), Class::Elf64);
    assert_eq!(elf.machine(), EM_X86_64);
    assert_eq!(elf.entry(), 0xFFFF_FFFF_8000_1000);
    assert!(!elf.is_pie(), "ET_EXEC is not position independent");
    elf.check_machine(EM_X86_64).unwrap();
    elf.validate_segments().unwrap();
    assert_eq!(elf.segments().count(), 2);
    assert_eq!(elf.loadable().count(), 2);
}

#[test]
fn decodes_segment_permissions() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();
    let loadable: Vec<Segment> = elf.loadable().collect();

    assert!(loadable[0].is_readable() && loadable[0].is_executable());
    assert!(!loadable[0].is_writable(), "text must not be writable");
    assert!(loadable[1].is_readable() && loadable[1].is_writable());
    assert!(!loadable[1].is_executable(), "data must not be executable");
}

#[test]
fn load_span_covers_bss_and_rounds_to_pages() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();
    let (low, high) = elf.load_span(0x1000).unwrap();

    assert_eq!(low, 0xFFFF_FFFF_8000_0000);
    // Second segment is 0x800 of file plus 0x800 of .bss, so it ends exactly on
    // a page and the span must include all of it.
    assert_eq!(high, 0xFFFF_FFFF_8000_2000);
}

#[test]
fn load_span_rounds_a_partial_page_up() {
    let image = Builder::new()
        .segment(PT_LOAD, PF_R, 0x1000, vec![0; 1], 0)
        .build();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(elf.load_span(0x1000), Some((0x1000, 0x2000)));
}

#[test]
fn segment_data_is_the_bytes_the_builder_wrote() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();
    let text = elf.loadable().next().unwrap();

    assert_eq!(text.data(&image).unwrap(), &[0x90u8; 0x1000][..]);
}

#[test]
fn rejects_a_truncated_image() {
    let image = kernel_image();
    for length in [0, 1, 4, 15, 16, EHDR_SIZE - 1] {
        assert!(
            Elf::parse(&image[..length]).is_err(),
            "a {length}-byte image must not parse"
        );
    }
}

#[test]
fn rejects_wrong_magic_class_and_endianness() {
    let good = kernel_image();

    let mut image = good.clone();
    image[1] = b'X';
    assert_eq!(Elf::parse(&image).unwrap_err(), ElfError::BadMagic);

    for class in [0u8, 3, 0xFF] {
        let mut image = good.clone();
        image[4] = class;
        assert_eq!(
            Elf::parse(&image).unwrap_err(),
            ElfError::UnsupportedClass(class),
            "class {class} is neither ELFCLASS32 nor ELFCLASS64"
        );
    }

    let mut image = good;
    image[5] = 2; // ELFDATA2MSB
    assert_eq!(Elf::parse(&image).unwrap_err(), ElfError::NotLittleEndian);
}

#[test]
fn rejects_the_wrong_architecture() {
    let image = Builder::new()
        .machine(EM_AARCH64)
        .segment(PT_LOAD, PF_R, 0x1000, vec![0; 16], 0)
        .build();
    let elf = Elf::parse(&image).unwrap();

    assert_eq!(
        elf.check_machine(EM_X86_64).unwrap_err(),
        ElfError::BadMachine(EM_AARCH64),
        "an AArch64 kernel must not load on x86-64"
    );
    elf.check_machine(EM_AARCH64).unwrap();
}

#[test]
fn rejects_an_object_that_is_neither_exec_nor_dyn() {
    let image = Builder::new()
        .elf_type(1) // ET_REL
        .segment(PT_LOAD, PF_R, 0x1000, vec![0; 16], 0)
        .build();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.check_machine(EM_X86_64).unwrap_err(),
        ElfError::BadType(1)
    );
}

#[test]
fn a_loader_that_does_not_relocate_refuses_a_position_independent_image() {
    let exec = kernel_image();
    Elf::parse(&exec).unwrap().check_fixed_address().unwrap();

    let pie = static_pie(Builder::new(), Vec::new(), &[]);
    let elf = Elf::parse(&pie).unwrap();
    elf.check_machine(EM_X86_64)
        .expect("a program loader, which relocates, still takes ET_DYN");
    assert_eq!(
        elf.check_fixed_address().unwrap_err(),
        ElfError::NotFixedAddress(ET_DYN),
        "the boot loader applies no relocations, so a PIE kernel would run unrelocated"
    );

    let object = Builder::new()
        .elf_type(1) // ET_REL
        .segment(PT_LOAD, PF_R, 0x1000, vec![0; 16], 0)
        .build();
    assert_eq!(
        Elf::parse(&object)
            .unwrap()
            .check_fixed_address()
            .unwrap_err(),
        ElfError::NotFixedAddress(1)
    );
}

#[test]
fn rejects_a_program_header_table_past_the_end() {
    let mut image = kernel_image();
    // Claim 4096 program headers; the table cannot be there.
    image[56..58].copy_from_slice(&4096u16.to_le_bytes());
    assert_eq!(Elf::parse(&image).unwrap_err(), ElfError::HeaderOutOfBounds);
}

#[test]
fn rejects_a_program_header_offset_that_overflows() {
    let mut image = kernel_image();
    image[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(Elf::parse(&image).unwrap_err(), ElfError::HeaderOutOfBounds);
}

#[test]
fn rejects_an_undersized_program_header_entry() {
    let mut image = kernel_image();
    image[54..56].copy_from_slice(&16u16.to_le_bytes());
    assert_eq!(Elf::parse(&image).unwrap_err(), ElfError::HeaderOutOfBounds);
}

/// Verifies: L.user.204
#[test]
fn rejects_segment_contents_past_the_end_of_the_image() {
    let mut image = kernel_image();
    // Inflate the first segment's p_filesz past the end of the image, and its
    // p_memsz with it -- otherwise `memsz < filesz` is the first thing wrong
    // and the malformed-size check fires before the bounds check we mean to
    // exercise here.
    let base = EHDR_SIZE + 32;
    image[base..base + 8].copy_from_slice(&0x10_0000u64.to_le_bytes());
    image[base + 8..base + 16].copy_from_slice(&0x10_0000u64.to_le_bytes());
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.validate_segments().unwrap_err(),
        ElfError::SegmentOutOfBounds
    );
}

#[test]
fn rejects_memsz_smaller_than_filesz() {
    let mut image = kernel_image();
    let base = EHDR_SIZE + 40; // p_memsz of the first segment
    image[base..base + 8].copy_from_slice(&1u64.to_le_bytes());
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.validate_segments().unwrap_err(),
        ElfError::SegmentMalformed,
        "a segment cannot occupy less memory than it has contents"
    );
}

#[test]
fn rejects_a_segment_whose_address_range_wraps() {
    let mut image = kernel_image();
    let base = EHDR_SIZE + 16; // p_vaddr of the first segment
    image[base..base + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.validate_segments().unwrap_err(),
        ElfError::SegmentMalformed
    );
    assert_eq!(elf.load_span(0x1000), None, "a wrapping span has no answer");
}

#[test]
fn an_image_with_no_loadable_segments_has_no_span() {
    let image = Builder::new()
        .segment(PT_NOTE, 0, 0x1000, vec![0; 8], 0)
        .build();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(elf.load_span(0x1000), None);
    assert_eq!(elf.loadable().count(), 0);
    elf.validate_segments().unwrap();
}

#[test]
fn maps_a_link_time_address_back_to_file_bytes() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();

    assert_eq!(
        elf.vaddr_to_bytes(0xFFFF_FFFF_8000_0000, 4),
        Some(&[0x90u8; 4][..])
    );
    assert_eq!(
        elf.vaddr_to_bytes(0xFFFF_FFFF_8000_1000, 4),
        Some(&[0xAAu8; 4][..])
    );
    // Inside the .bss tail: no file bytes exist for it.
    assert_eq!(elf.vaddr_to_bytes(0xFFFF_FFFF_8000_1900, 4), None);
    assert_eq!(elf.vaddr_to_bytes(0x1234, 4), None, "unmapped address");
}

#[test]
fn an_image_without_a_dynamic_segment_needs_no_relocation() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(elf.relocations().unwrap().count(), 0);
}

#[test]
fn reads_relative_relocations_from_a_static_pie() {
    let mut table = Vec::new();
    for (offset, addend) in [(0x2000u64, 0x2100i64), (0x2008, 0x2200)] {
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&(R_X86_64_RELATIVE as u64).to_le_bytes());
        table.extend_from_slice(&addend.to_le_bytes());
    }
    let size = table.len() as u64;
    let image = static_pie(
        Builder::new(),
        table,
        &[
            (DT_RELA, PIE_BASE),
            (DT_RELASZ, size),
            (DT_RELAENT, RELA_SIZE as u64),
            (DT_NULL, 0),
        ],
    );

    let elf = Elf::parse(&image).unwrap();
    assert!(elf.is_pie());

    let relocations: Vec<Relocation> = elf.relocations().unwrap().map(Result::unwrap).collect();

    assert_eq!(
        relocations,
        [
            Relocation {
                offset: 0x2000,
                addend: Addend::Explicit(0x2100)
            },
            Relocation {
                offset: 0x2008,
                addend: Addend::Explicit(0x2200)
            },
        ]
    );

    // With a load bias, both the patch site and the value shift by it -- and
    // what is already stored at the target is irrelevant to a RELA entry.
    let bias = 0x40_0000u64;
    assert_eq!(relocations[0].target(bias), 0x2000 + bias);
    assert_eq!(relocations[0].value(bias, 0xDEAD_BEEF), 0x2100 + bias);
}

#[test]
fn relocation_helpers_wrap_rather_than_panic() {
    let relocation = Relocation {
        offset: u64::MAX,
        addend: Addend::Explicit(-1),
    };
    assert_eq!(relocation.target(1), 0, "a wrapping target is not a panic");
    assert_eq!(relocation.value(1, 0), 0);

    let in_place = Relocation {
        offset: 0,
        addend: Addend::InPlace,
    };
    assert_eq!(in_place.value(u64::MAX, 2), 1);
}

// ---------------------------------------------------------------------------
// 32-bit images
// ---------------------------------------------------------------------------

#[test]
fn parses_a_well_formed_arm32_image() {
    let image = arm32_kernel_image();
    let elf = Elf::parse(&image).unwrap();

    assert_eq!(elf.class(), Class::Elf32);
    assert_eq!(elf.machine(), EM_ARM);
    assert_eq!(elf.entry(), 0xF000_0000);
    elf.check_machine(EM_ARM).unwrap();
    elf.validate_segments().unwrap();
    assert_eq!(
        elf.check_machine(EM_AARCH64).unwrap_err(),
        ElfError::BadMachine(EM_ARM),
        "a 32-bit Arm kernel must not load on AArch64"
    );

    let loadable: Vec<Segment> = elf.loadable().collect();
    assert_eq!(loadable.len(), 2);
    assert_eq!(loadable[0].vaddr, 0xF000_0000);
    assert!(loadable[0].is_executable() && !loadable[0].is_writable());
    assert_eq!(loadable[1].filesz, 0x600);
    assert_eq!(loadable[1].memsz, 0x1000, "the .bss tail survives widening");
    assert!(loadable[1].is_writable() && !loadable[1].is_executable());
    assert_eq!(loadable[0].data(&image).unwrap(), &[0xE3u8; 0x1000][..]);
    assert_eq!(elf.load_span(0x1000), Some((0xF000_0000, 0xF000_2000)));
}

#[test]
fn an_arm32_header_is_read_from_its_own_offsets() {
    let image = arm32_kernel_image();
    let header = *Elf::parse(&image).unwrap().header();

    // The two classes share e_type and e_machine and nothing after them. A
    // reader that used the ELF64 offsets here would take e_shoff for e_phoff
    // and a flags word for the entry point.
    assert_eq!(header.phoff, EHDR32_SIZE as u64);
    assert_eq!(header.phentsize, PHDR32_SIZE as u16);
    assert_eq!(header.phnum, 2);
    assert_eq!(header.elf_type, ET_EXEC);
}

#[test]
fn rejects_a_truncated_arm32_header() {
    let image = arm32_kernel_image();
    for length in [16, 24, 40, EHDR32_SIZE - 1] {
        assert_eq!(
            Elf::parse(&image[..length]).unwrap_err(),
            ElfError::TooShort,
            "a {length}-byte ELF32 image must not parse"
        );
    }
}

#[test]
fn rejects_an_undersized_arm32_program_header_entry() {
    let mut image = arm32_kernel_image();
    image[42..44].copy_from_slice(&16u16.to_le_bytes());
    assert_eq!(Elf::parse(&image).unwrap_err(), ElfError::HeaderOutOfBounds);
}

#[test]
fn a_32_bit_segment_may_not_wrap_four_gibibytes() {
    // Ends at 0x1_0000_1000: no overflow in the u64 this crate widens every
    // address to, and a wrap on the machine the image was built for.
    let image = Builder::arm32()
        .segment(PT_LOAD, PF_R, 0xFFFF_F000, vec![0; 16], 0x1FF0)
        .build();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.validate_segments().unwrap_err(),
        ElfError::SegmentMalformed
    );
    assert_eq!(elf.load_span(0x1000), None);

    // The same numbers in a 64-bit image are an ordinary segment.
    let image = Builder::new()
        .segment(PT_LOAD, PF_R, 0xFFFF_F000, vec![0; 16], 0x1FF0)
        .build();
    Elf::parse(&image).unwrap().validate_segments().unwrap();
}

#[test]
fn a_32_bit_segment_may_end_exactly_at_four_gibibytes() {
    let image = Builder::arm32()
        .segment(PT_LOAD, PF_R, 0xFFFF_F000, vec![0; 16], 0xFF0)
        .build();
    let elf = Elf::parse(&image).unwrap();
    elf.validate_segments().unwrap();
    assert_eq!(elf.load_span(0x1000), Some((0xFFFF_F000, 0x1_0000_0000)));
}

/// A `REL` table of `R_ARM_RELATIVE` entries at the given offsets, with the
/// symbol index in each `r_info` set to `symbol` to prove it is ignored.
fn arm_rel_table(entries: &[(u32, u32)], symbol: u32) -> Vec<u8> {
    let mut table = Vec::new();
    for &(offset, kind) in entries {
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&((symbol << 8) | kind).to_le_bytes());
    }
    table
}

#[test]
fn reads_in_place_relocations_from_a_32_bit_static_pie() {
    let table = arm_rel_table(&[(0x2000, R_ARM_RELATIVE), (0x2004, R_ARM_RELATIVE)], 0);
    let size = table.len() as u64;
    let image = static_pie(
        Builder::arm32(),
        table,
        &[
            (DT_REL, PIE_BASE),
            (DT_RELSZ, size),
            (DT_RELENT, REL32_SIZE as u64),
            (DT_NULL, 0),
        ],
    );

    let elf = Elf::parse(&image).unwrap();
    let relocations: Vec<Relocation> = elf.relocations().unwrap().map(Result::unwrap).collect();
    assert_eq!(
        relocations,
        [
            Relocation {
                offset: 0x2000,
                addend: Addend::InPlace
            },
            Relocation {
                offset: 0x2004,
                addend: Addend::InPlace
            },
        ],
        "a REL entry has no addend of its own"
    );

    // The linker stored the link-time value at the target; relocating adds
    // the bias to it.
    let bias = 0x4000_0000u64;
    assert_eq!(relocations[1].target(bias), 0x4000_2004);
    assert_eq!(relocations[1].value(bias, 0x0000_3180), 0x4000_3180);
}

#[test]
fn an_absent_entry_size_means_the_formats_own() {
    let table = arm_rel_table(&[(0x2000, R_ARM_RELATIVE)], 0);
    let size = table.len() as u64;
    let image = static_pie(
        Builder::arm32(),
        table,
        &[(DT_REL, PIE_BASE), (DT_RELSZ, size), (DT_NULL, 0)],
    );
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(elf.relocations().unwrap().count(), 1);
}

#[test]
fn the_relocation_type_is_the_low_byte_of_an_elf32_info() {
    // Symbol 5 in the upper bits: a relative relocation names no symbol, but
    // a reader that took the whole word as the type would reject it.
    let table = arm_rel_table(&[(0x2000, R_ARM_RELATIVE), (0x2004, 2)], 5);
    let size = table.len() as u64;
    let image = static_pie(
        Builder::arm32(),
        table,
        &[(DT_REL, PIE_BASE), (DT_RELSZ, size), (DT_NULL, 0)],
    );
    let elf = Elf::parse(&image).unwrap();
    let results: Vec<Result<Relocation, ElfError>> = elf.relocations().unwrap().collect();

    assert!(results[0].is_ok(), "R_ARM_RELATIVE with a symbol index");
    assert_eq!(
        results[1],
        Err(ElfError::UnsupportedRelocation(2)),
        "R_ARM_ABS32 needs a symbol table this loader does not have"
    );
}

#[test]
fn a_32_bit_rela_table_carries_its_addends() {
    let mut table = Vec::new();
    table.extend_from_slice(&0x2000u32.to_le_bytes());
    table.extend_from_slice(&R_ARM_RELATIVE.to_le_bytes());
    table.extend_from_slice(&(-8i32).to_le_bytes());
    let size = table.len() as u64;
    let image = static_pie(
        Builder::arm32(),
        table,
        &[(DT_RELA, PIE_BASE), (DT_RELASZ, size), (DT_NULL, 0)],
    );
    let elf = Elf::parse(&image).unwrap();
    let relocation = elf.relocations().unwrap().next().unwrap().unwrap();
    assert_eq!(relocation.addend, Addend::Explicit(-8), "sign extended");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// Exercise every accessor on one image, requiring only that none of them
/// panics. Pulled out of the loop below so the nesting stays readable.
fn poke_at_everything(image: &[u8]) {
    let Ok(elf) = Elf::parse(image) else {
        return;
    };
    let _ = elf.validate_segments();
    let _ = elf.load_span(0x1000);
    let _ = elf.check_machine(EM_X86_64);
    let _ = elf.check_machine(EM_ARM);
    let _ = elf.vaddr_to_bytes(0xFFFF_FFFF_8000_0000, 8);
    let _ = elf.vaddr_to_bytes(0xF000_0000, 8);

    for segment in elf.segments() {
        let _ = segment.data(image);
        let _ = segment.vaddr_end();
    }

    if let Ok(relocations) = elf.relocations() {
        for relocation in relocations.take(64) {
            let _ = relocation;
        }
    }
}

#[test]
fn parsing_never_panics_on_arbitrary_bytes() {
    // The cheap deterministic version of the fuzz target: walk a known-good
    // image byte by byte, corrupting each in turn, and require that every
    // accessor either answers or errors. The fuzzer searches further; this runs
    // on every commit. Miri, whose question is the accessors' indexing
    // rather than which values reach it, tries the two extremes: all five
    // were most of this crate's time under Miri.
    let patches: &[u8] = if cfg!(miri) {
        &[0x00, 0xFF]
    } else {
        &[0x00, 0x01, 0x02, 0x7F, 0xFF]
    };
    for good in [kernel_image(), arm32_kernel_image()] {
        for index in 0..good.len().min(512) {
            for &patch in patches {
                let mut image = good.clone();
                image[index] = patch;
                poke_at_everything(&image);
            }
        }
    }
}

#[test]
fn an_image_naming_no_interpreter_answers_none() {
    let image = kernel_image();
    let elf = Elf::parse(&image).unwrap();
    assert!(
        elf.interpreter().is_none(),
        "a static image asks for no dynamic linker"
    );
}

#[test]
fn reads_the_interpreter_path_without_its_nul() {
    let image = Builder::new()
        .elf_type(ET_DYN)
        .segment(PT_LOAD, PF_R | PF_X, 0x1000, vec![0x90; 0x100], 0)
        .segment(
            PT_INTERP,
            PF_R,
            0x2000,
            b"/lib/ld-ferrix.so.1\0".to_vec(),
            0,
        )
        .build();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(elf.interpreter(), Some(Ok(&b"/lib/ld-ferrix.so.1"[..])));
}

#[test]
fn an_interpreter_inside_the_segment_that_maps_it_is_read() {
    // How a linked binary really carries it: the `PT_INTERP` is a window into
    // the read-execute `PT_LOAD`, not bytes of its own.
    let mut text = vec![0x90; 0x100];
    text[0x20..0x20 + 9].copy_from_slice(b"/lib/ld\0\0");
    let image = Builder::new()
        .elf_type(ET_DYN)
        .segment(PT_LOAD, PF_R | PF_X, 0x1000, text, 0)
        .within(PT_INTERP, 0x1020, 0, 0x20, 8)
        .build();
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(elf.interpreter(), Some(Ok(&b"/lib/ld"[..])));
}

#[test]
fn an_interpreter_that_is_not_a_path_is_refused_and_not_ignored() {
    // Each of these asks for an interpreter and fails to say which, which is
    // not the same as asking for none: the image cannot be run either way, but
    // only one of the two answers is honest about why.
    for (what, data) in [
        ("empty", vec![]),
        ("unterminated", b"/lib/ld".to_vec()),
        ("only a NUL", vec![0]),
        ("a NUL before the end", b"/lib\0/ld\0".to_vec()),
    ] {
        let image = Builder::new()
            .elf_type(ET_DYN)
            .segment(PT_LOAD, PF_R | PF_X, 0x1000, vec![0x90; 0x100], 0)
            .segment(PT_INTERP, PF_R, 0x2000, data, 0)
            .build();
        let elf = Elf::parse(&image).unwrap();
        assert_eq!(
            elf.interpreter(),
            Some(Err(ElfError::BadInterpreter)),
            "a {what} PT_INTERP was not refused"
        );
    }
}

#[test]
fn an_interpreter_segment_outside_the_image_is_refused() {
    let mut image = Builder::new()
        .elf_type(ET_DYN)
        .segment(PT_LOAD, PF_R | PF_X, 0x1000, vec![0x90; 0x100], 0)
        .segment(PT_INTERP, PF_R, 0x2000, b"/lib/ld\0".to_vec(), 0)
        .build();
    // Reach past the end of the file, which `Segment::data` is what refuses.
    let phdr = EHDR_SIZE + PHDR_SIZE;
    image[phdr + 32..phdr + 40].copy_from_slice(&u64::MAX.to_le_bytes());
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.interpreter(),
        Some(Err(ElfError::SegmentOutOfBounds)),
        "a PT_INTERP running past the image was not refused"
    );
}

#[test]
fn the_headers_are_the_file_header_and_the_program_header_table() {
    let image = kernel_image();
    let end = (EHDR_SIZE + 2 * PHDR_SIZE) as u64;
    // Only the file header is needed to say how much more to read.
    assert_eq!(Elf::headers_len(&image[..EHDR_SIZE]), Ok(end));
    assert_eq!(Elf::headers_len(&image), Ok(end));
    let arm = arm32_kernel_image();
    assert_eq!(
        Elf::headers_len(&arm[..EHDR32_SIZE]),
        Ok((EHDR32_SIZE + 2 * PHDR32_SIZE) as u64)
    );

    // Exactly that much parses, and one byte less does not.
    let headers = &image[..end as usize];
    assert!(Elf::parse(headers).is_ok());
    assert_eq!(
        Elf::parse(&image[..end as usize - 1]).unwrap_err(),
        ElfError::HeaderOutOfBounds
    );
}

#[test]
fn an_image_with_no_program_headers_needs_only_its_file_header() {
    let mut image = kernel_image();
    image[56..58].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(Elf::headers_len(&image), Ok(EHDR_SIZE as u64));
}

#[test]
fn the_headers_length_refuses_what_parse_refuses() {
    assert_eq!(Elf::headers_len(&[0x7F, b'E']), Err(ElfError::TooShort));
    let mut image = kernel_image();
    image[0] = 0;
    assert_eq!(Elf::headers_len(&image), Err(ElfError::BadMagic));

    let mut image = kernel_image();
    image[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        Elf::headers_len(&image),
        Err(ElfError::HeaderOutOfBounds),
        "a table whose end wraps has no length"
    );
    let mut image = kernel_image();
    image[54..56].copy_from_slice(&16u16.to_le_bytes());
    assert_eq!(
        Elf::headers_len(&image),
        Err(ElfError::HeaderOutOfBounds),
        "entries too small to be program headers"
    );
}

#[test]
fn segments_are_validated_against_the_file_the_headers_began() {
    let image = kernel_image();
    let end = Elf::headers_len(&image).unwrap() as usize;
    let headers = Elf::parse(&image[..end]).unwrap();
    // The headers alone cannot hold the segments' contents, and need not.
    assert_eq!(
        headers.validate_segments().unwrap_err(),
        ElfError::SegmentOutOfBounds
    );
    headers
        .validate_segments_within(image.len() as u64)
        .unwrap();
    assert_eq!(
        headers
            .validate_segments_within(image.len() as u64 - 1)
            .unwrap_err(),
        ElfError::SegmentOutOfBounds,
        "a file one byte short of the last segment's contents"
    );

    let mut image = kernel_image();
    let base = EHDR_SIZE + 40; // p_memsz of the first segment
    image[base..base + 8].copy_from_slice(&1u64.to_le_bytes());
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.validate_segments_within(u64::MAX).unwrap_err(),
        ElfError::SegmentMalformed,
        "the size checks do not depend on where the contents are"
    );

    let mut image = kernel_image();
    let base = EHDR_SIZE + 8; // p_offset of the first segment
    image[base..base + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let elf = Elf::parse(&image).unwrap();
    assert_eq!(
        elf.validate_segments_within(u64::MAX).unwrap_err(),
        ElfError::SegmentOutOfBounds,
        "contents whose end wraps are nowhere in any file"
    );
}

#[test]
fn an_interpreter_read_from_the_file_is_checked_as_the_image_would_check_it() {
    assert_eq!(interpreter_name(b"/lib/ld\0"), Ok(&b"/lib/ld"[..]));
    for data in [&b""[..], b"/lib/ld", b"\0", b"/lib\0/ld\0"] {
        assert_eq!(interpreter_name(data), Err(ElfError::BadInterpreter));
    }
}
