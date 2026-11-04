/*
 * The layout of a program of a shared process's region: native.ld at the region's
 * window (docs/microkernel/K11.md §2.1), which is otherwise the same.
 *
 * What the kernel's ELF loader (src/kernel/src/syscall/load.rs) requires:
 *
 *   - ET_EXEC at a fixed address. A position-independent image is refused
 *     by name, because nothing applies relocations to a program yet; the
 *     targets' `-C relocation-model=static` in .cargo/config.toml gives this.
 *   - No page both writable and executable. Text, read-only data and
 *     writable data are three segments, each starting on its own page, so no
 *     page is shared between two of them.
 *   - An entry point in user space. 4 MiB is above MMAP_MIN_ADDR (64 KiB),
 *     well inside the user half of both a 32-bit and a 48-bit space, and where
 *     a static Linux program is conventionally linked.
 *
 * No program headers are loaded, no interpreter is named and there is no
 * dynamic section: a native program is static, and the xtask build step
 * checks each one for exactly this before it goes into the initramfs.
 */

ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);  /* R-X */
    rodata PT_LOAD FLAGS(4);  /* R-- */
    data   PT_LOAD FLAGS(6);  /* RW- */
}

SECTIONS
{
    . = 0x700000400000;

    .text : {
        *(.text._start)
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata : {
        *(.rodata .rodata.*)
        *(.data.rel.ro .data.rel.ro.*)
    } :rodata
    /* ARMv7-A's unwind index, which the compiler emits even for panic=abort. */
    .ARM.exidx : {
        *(.ARM.exidx .ARM.exidx.*)
    } :rodata

    . = ALIGN(4096);
    .data : {
        *(.data .data.*)
    } :data
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.interp .dynamic .dynsym .dynstr .rela.* .rel.*)
    }
}
