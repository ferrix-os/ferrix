//! Fuzz the ELF64 reader.
//!
//! This is the sharpest surface in the tree that takes bytes somebody else
//! chose. Today the loader parses the kernel with it; from stage 6 the kernel
//! parses *user programs* with it, which means an attacker picks every byte and
//! the parse happens in ring 0 with nothing above it to contain a mistake.
//!
//! The contract being fuzzed is total: for **any** input, every accessor either
//! answers or returns an error. It must never panic, never index out of bounds,
//! never overflow, and never read outside the slice it was given.
//! `libs/elf` is `#![forbid(unsafe_code)]`, so a memory-safety bug here would
//! have to be a compiler bug — what this actually hunts is the panic, which in
//! a kernel is just as fatal.

#![no_main]

use ferrix_elf::{EM_AARCH64, EM_X86_64, Elf};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(elf) = Elf::parse(data) else {
        return;
    };

    // Header accessors: cheap, and a panic in one would be a parse that
    // validated something it had not read.
    let _ = elf.entry();
    let _ = elf.machine();
    let _ = elf.elf_type();
    let _ = elf.is_pie();
    let _ = elf.check_machine(EM_X86_64);
    let _ = elf.check_machine(EM_AARCH64);

    // The two the loader calls before it maps anything.
    let _ = elf.validate_segments();
    let _ = elf.load_span(4096);

    // Every segment, and the bytes behind it.
    for segment in elf.segments() {
        let _ = segment.data(data);
        let _ = segment.vaddr_end();
        let _ = segment.is_load();
        let _ = segment.is_executable();

        // Resolving a link-time address back to file bytes, at an address the
        // image itself claims to contain.
        let _ = elf.vaddr_to_bytes(segment.vaddr, 8);
    }

    // Addresses the image did *not* claim, including the edges.
    for probe in [0u64, 1, u64::MAX, u64::MAX - 7, 0xFFFF_FFFF_8000_0000] {
        let _ = elf.vaddr_to_bytes(probe, 8);
        let _ = elf.vaddr_to_bytes(probe, u64::MAX);
    }

    // Relocations. Bounded, because a malformed DT_RELASZ can describe an
    // arbitrarily long table and the fuzzer should spend its time finding new
    // shapes rather than walking one.
    if let Ok(relocations) = elf.relocations() {
        for relocation in relocations.take(4096) {
            if let Ok(relocation) = relocation {
                let _ = relocation.target(0x40_0000);
                let _ = relocation.value(0x40_0000);
                // A bias that wraps: the helpers promise wrapping arithmetic,
                // not a panic, and release builds of the kernel have overflow
                // checks on.
                let _ = relocation.target(u64::MAX);
                let _ = relocation.value(u64::MAX);
            }
        }
    }
});
