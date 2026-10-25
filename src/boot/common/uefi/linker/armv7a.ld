/* Link script for the ARMv7-A loader.
 *
 * The other two loaders come out of rustc as PE32+ images and need no script.
 * There is no ARMv7 UEFI target, so this one is linked as an ELF32 static PIE
 * and turned into a PE32 UEFI application by tools/common/xtask/src/pe.rs -- and that
 * converter's input contract is this file:
 *
 *   * three PT_LOADs -- read-execute, read-only, read-write -- each starting
 *     on a page and each directly after the last, so that each becomes one PE
 *     section with one set of permissions and the sections are adjacent, as a
 *     PE image's must be;
 *   * nothing below 0x1000, so the first page of the PE image can hold its
 *     headers;
 *   * a PT_DYNAMIC naming the relocation table, whose R_ARM_RELATIVE entries
 *     become the PE base relocations firmware applies when it loads us.
 *
 * The converter checks every one of these and refuses an image that breaks
 * them, so a change here that does fails the build rather than the boot.
 */

ENTRY(efi_main)

PHDRS
{
    text    PT_LOAD FLAGS(5);     /* read + execute */
    rodata  PT_LOAD FLAGS(4);     /* read only */
    data    PT_LOAD FLAGS(6);     /* read + write */
    dynamic PT_DYNAMIC FLAGS(6);
}

SECTIONS
{
    . = 0x1000;

    .text : ALIGN(4096) {
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    } :rodata
    /* Read by the converter, not by firmware: it is copied into the image
     * because it is inside a segment, and costs a few hundred bytes. */
    /* The trust-policy block (docs/SIGNING.md §10.6), a section of its
     * own in the ELF, read-only; xtask finds it there by name. */
    .fxanch : ALIGN(16) { KEEP(*(.fxanch)) } :rodata
    .rel.dyn  : { *(.rel.dyn) }  :rodata
    .dynsym   : { *(.dynsym) }   :rodata
    .dynstr   : { *(.dynstr) }   :rodata
    .hash     : { *(.hash) }     :rodata
    .gnu.hash : { *(.gnu.hash) } :rodata

    . = ALIGN(4096);
    .data : ALIGN(4096) {
        *(.data.rel.ro .data.rel.ro.*)
        *(.data .data.*)
        *(.got) *(.got.plt)
    } :data
    .dynamic : { *(.dynamic) } :data :dynamic

    /* Firmware zero-fills a section past its raw data, which is how a PE
     * image spells .bss. */
    .bss (NOLOAD) : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.ARM.exidx*) *(.ARM.extab*)
        *(.eh_frame*)
        *(.comment) *(.note*) *(.interp)
    }
}
