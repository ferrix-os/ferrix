/*
 * nvrm-core: RM's core (nv-kernel.o) linked alone, at 0x4000_0000, for
 * nvrm to load from the NVIDIA volume (docs/NVIDIA.md §4.1, "The core").
 *
 * Three loadable segments and the stack note, nothing else: read-only
 * (the export header nvrm reads first, then .rodata), executable (.text)
 * and writable (.data, .bss). None is both writable and executable, and
 * the Makefile and nvrm's loader each refuse one that is. The core is
 * built -mcmodel=kernel, so everything here must stay below 2 GiB, which
 * the loader checks too.
 *
 * The core's imports come in as nvrm's addresses, from the generated
 * nvrm-imports.ld given beside this script.
 *
 * SPDX-License-Identifier: MIT
 */
OUTPUT_FORMAT("elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(nvrm_core_header)

PHDRS
{
	rodata PT_LOAD FLAGS(4);
	text   PT_LOAD FLAGS(5);
	data   PT_LOAD FLAGS(6);
	stack  PT_GNU_STACK FLAGS(6);
}

SECTIONS
{
	. = 0x40000000;
	.nvrm_core_header : { KEEP(*(.nvrm_core_header)) } :rodata
	.rodata : { *(.rodata .rodata.*) } :rodata

	. = ALIGN(0x1000);
	.text : { *(.text .text.*) } :text

	. = ALIGN(0x1000);
	.data : { *(.data .data.*) } :data
	.bss : { *(.bss .bss.*) *(COMMON) } :data

	/DISCARD/ : { *(.comment) *(.note .note.*) *(.eh_frame .eh_frame_hdr) }
}
