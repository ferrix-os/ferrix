//! The hand-off ABI between the UEFI loader and the kernel.
//!
//! These are two separate programs, linked for different targets, that meet at
//! exactly one struct. Both compile it from this one definition, every type is
//! `repr(C)`, and the kernel refuses to start if the magic or version disagree
//! — because the failure mode of a silent layout mismatch is a triple fault
//! with nothing on the serial port to say why.
//!
//! # One layout per word width
//!
//! Where things live in the virtual address space is a [`Layout`], and there
//! are two: [`LAYOUT_64`], which x86-64 and AArch64 share constant for
//! constant, and [`LAYOUT_32`] for ARMv7-A, which cannot hold a single one of
//! them. The crate root re-exports the one matching the target's pointer width
//! under the names the rest of the tree uses, so generic code says
//! [`PHYSMAP_BASE`] and never a width. Both layouts are checked at compile time
//! on every build, and the host tests read both — which is how the 32-bit one
//! is tested by a machine that is not 32-bit.
//!
//! [`BootInfo`] itself has *one* layout on every width. Its addresses are
//! `u64` even where a pointer would be four bytes, so the structure the host
//! tests build is the structure a 32-bit kernel reads.
//!
//! # Reading it safely
//!
//! [`BootInfo`] is a raw structure full of addresses the kernel did not
//! create. Rather than sprinkle `unsafe` over every reader, the kernel
//! validates it once through [`BootInfo::validate`] and works with the
//! resulting [`BootView`], whose accessors are safe.
//!
//! ```
//! # use ferrix_bootinfo::{BootInfo, BootInfoError};
//! # fn example(raw: *const BootInfo) -> Result<(), BootInfoError> {
//! // SAFETY: in the kernel this pointer is the one argument the loader passed.
//! let view = unsafe { (*raw).validate() }?;
//! for region in view.regions() {
//!     let _ = region.kind;
//! }
//! # Ok(())
//! # }
//! ```

#![no_std]

use core::fmt;

mod kaslr;
pub mod pstore;
pub use kaslr::{
    KASLR_DECLINED, KASLR_FIXED_IMAGE, KASLR_MOVED, KASLR_NO_ENTROPY, KASLR_NOT_OFFERED,
    KERNEL_TOP_GUARD, Kaslr, SOURCE_COUNTER, SOURCE_CPU_RNG, SOURCE_FIRMWARE_RNG, SOURCE_NONE,
    SOURCE_SMCCC_TRNG, Slots,
};

/// Magic number identifying a [`BootInfo`], ASCII `FERRIXBI`.
///
/// Checked by the kernel before it touches anything else in the structure.
pub const BOOTINFO_MAGIC: u64 = 0x4645_5252_4958_4249;

/// Layout version of [`BootInfo`], bumped on any change to this file.
///
/// Version 2 added the physical origin of the direct map and the length of the
/// device tree, and turned the two pointers into addresses so the structure
/// has the same layout on every word width. Version 3 added the loader's
/// identity mapping of its own image inside the kernel's tree, which a board
/// whose RAM is above the split needs and which the kernel has to take down.
/// Version 4 added [`Framebuffer::reclaimable`], because firmware can leave a
/// framebuffer in boot-services memory the kernel hands out again. Version 5
/// added what firmware knows that the kernel cannot find out for itself: the
/// time of day, and random bytes. HTTPS needs both, and so does anything else
/// that checks a certificate or makes a key. Version 6 added
/// [`BootInfo::firmware_seed_len`], because a phone's bootloader gives fewer
/// random bytes than the seed holds, and the kernel credits what it was
/// given, not what the field can hold. Version 7 added [`BootInfo::kaslr`],
/// and let the direct map begin anywhere in its region rather than at its
/// base: the loader moves the kernel image, the direct map and the top of the
/// vmap arena at random each boot, and says how, and how well. Version 8
/// added [`BootInfo::pstore_phys`] and [`BootInfo::pstore_len`], the region
/// the kernel keeps its log in across a reset ([`pstore`]), and
/// [`MemKind::Pstore`]. Version 9 added [`BootInfo::smbios`] and
/// [`BootInfo::smbios_len`]: the loader's copy of firmware's SMBIOS entry
/// point and table (`docs/DMI.md`), and [`MemKind::Smbios`].
pub const BOOTINFO_VERSION: u32 = 9;

/// Bytes at the start of the loader's SMBIOS copy that hold the entry point,
/// zero-padded past its own length; the structure table follows them.
pub const SMBIOS_ENTRY_BYTES: u64 = 32;

/// [`BootInfo::firmware_flags`]: [`BootInfo::firmware_time`] holds the time
/// firmware's `GetTime` gave.
pub const FIRMWARE_TIME: u64 = 1 << 0;

/// [`BootInfo::firmware_flags`]: [`BootInfo::firmware_seed`] holds random bytes
/// from firmware, [`BootInfo::firmware_seed_len`] of them: `EFI_RNG_PROTOCOL`'s,
/// or what a phone's bootloader left in the device tree's `/chosen`.
pub const FIRMWARE_SEED: u64 = 1 << 1;

/// A calendar time as firmware's clock reports it, `offset_minutes` east of
/// UTC, as nanoseconds since the Unix epoch; `None` for a date that does not
/// exist or does not fit.
///
/// Days are counted with the proleptic Gregorian calendar, by Howard
/// Hinnant's `days_from_civil`, which is exact for every year an `i64` of
/// nanoseconds spans.
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "the fields of EFI_TIME, one argument each, as firmware gives them"
)]
pub fn unix_nanos(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
    offset_minutes: i16,
) -> Option<i64> {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > month_days || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if nanosecond >= 1_000_000_000 {
        return None;
    }
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let shifted_month = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)
            - i64::from(offset_minutes) * 60;
    seconds
        .checked_mul(1_000_000_000)?
        .checked_add(i64::from(nanosecond))
}

// ---------------------------------------------------------------------------
// Virtual memory layout
// ---------------------------------------------------------------------------

/// Where everything lives in the virtual address space, for one word width.
///
/// A user half at the bottom, and three kernel regions above it: the direct map
/// of physical RAM, the dynamic mapping area, and the kernel image. The 64-bit
/// layout puts them in that order; the 32-bit one puts the vmap area first,
/// because 4 GiB is small enough that which region gets the address space left
/// over from the others is a decision rather than a detail — and it is the
/// direct map, which is what decides how much RAM a 32-bit kernel can use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// Width of a virtual address.
    pub address_bits: u32,
    /// Highest user virtual address, exclusive.
    pub user_end: u64,
    /// Lowest address belonging to the kernel.
    pub kernel_half: u64,
    /// Base of the direct map of physical RAM.
    pub physmap_base: u64,
    /// End of the direct map, exclusive.
    pub physmap_end: u64,
    /// Base of the dynamic mapping area: `MMIO` windows, guard-paged kernel
    /// stacks, anything whose virtual address is chosen at runtime.
    pub vmap_base: u64,
    /// Size of the dynamic mapping area, in bytes.
    pub vmap_size: u64,
    /// Bytes at the bottom of the dynamic mapping area kept out of the
    /// allocator for the fixed windows early boot places there before there is
    /// an allocator: the console, a framebuffer, stage 3's on-demand window.
    pub vmap_reserved: u64,
    /// Where the kernel image is linked. Everything from here to the top of
    /// the address space is the image's.
    pub kernel_base: u64,
}

impl Layout {
    /// One past the last address of the dynamic mapping area.
    #[must_use]
    pub const fn vmap_end(&self) -> u64 {
        self.vmap_base + self.vmap_size
    }

    /// Bytes of physical memory the direct map can cover.
    #[must_use]
    pub const fn physmap_size(&self) -> u64 {
        self.physmap_end - self.physmap_base
    }

    /// The first rule this layout breaks, or `None` if it is consistent.
    ///
    /// A description rather than a boolean, so the test that runs this on both
    /// layouts can say which rule failed.
    #[must_use]
    pub const fn violation(&self) -> Option<&'static str> {
        let mask = PAGE_SIZE - 1;
        if (self.user_end | self.kernel_half | self.physmap_base | self.physmap_end) & mask != 0
            || (self.vmap_base | self.vmap_size | self.vmap_reserved | self.kernel_base) & mask != 0
        {
            return Some("every boundary must be page aligned");
        }
        if self.user_end > self.kernel_half {
            return Some("the user half must end before the kernel half begins");
        }
        if self.physmap_base >= self.physmap_end {
            return Some("the direct map must span a positive range");
        }
        if self.physmap_base < self.kernel_half || self.vmap_base < self.kernel_half {
            return Some("the direct map and the vmap area are the kernel's");
        }
        if self.vmap_reserved >= self.vmap_size {
            return Some("the vmap area must have room beyond its reserved windows");
        }
        if !(self.physmap_end <= self.vmap_base || self.vmap_end() <= self.physmap_base) {
            return Some("the direct map must not overlap the vmap area");
        }
        if self.physmap_end > self.kernel_base || self.vmap_end() > self.kernel_base {
            return Some("the kernel image must be above everything else");
        }
        if self.address_bits < 64 && self.kernel_base >= 1 << self.address_bits {
            return Some("the kernel image must be inside the address space");
        }
        // The arena's top may move down by up to this much, and the arena
        // left has to stay most of what it was.
        if (self.vmap_slots() - 1) * self.vmap_granule() > (self.vmap_size - self.vmap_reserved) / 8
        {
            return Some("the vmap arena's top may move by at most an eighth of the arena");
        }
        if self.kernel_ceiling() <= self.kernel_base {
            return Some("the kernel image's region must be above the guard at the top");
        }
        None
    }
}

/// The layout x86-64 and AArch64 share.
///
/// The image is in the top -2 GiB, which is what the x86-64 "kernel" code
/// model addresses. AArch64 does not require that and uses it anyway, because
/// one layout across both is one set of bugs instead of two.
pub const LAYOUT_64: Layout = Layout {
    address_bits: 64,
    user_end: 0x0000_8000_0000_0000,
    kernel_half: 0xFFFF_8000_0000_0000,
    physmap_base: 0xFFFF_8000_0000_0000,
    physmap_end: 0xFFFF_FF00_0000_0000,
    vmap_base: 0xFFFF_FF00_0000_0000,
    vmap_size: 0x0000_00EF_0000_0000,
    vmap_reserved: 0x1_0000_0000,
    kernel_base: 0xFFFF_FFFF_8000_0000,
};

/// The layout of ARMv7-A: a 2/2 split of 4 GiB.
///
/// Two gibibytes of user space, because a 32-bit process wants the larger
/// half; then 512 MiB of vmap, 1.25 GiB of direct map — the ceiling on RAM
/// this kernel can use, and more than the boards it targets carry — and
/// 256 MiB for the image at the top.
pub const LAYOUT_32: Layout = Layout {
    address_bits: 32,
    user_end: 0x8000_0000,
    kernel_half: 0x8000_0000,
    vmap_base: 0x8000_0000,
    vmap_size: 0x2000_0000,
    vmap_reserved: 0x0400_0000,
    physmap_base: 0xA000_0000,
    physmap_end: 0xF000_0000,
    kernel_base: 0xF000_0000,
};

// Compile-time rather than tests on purpose: an overlap between the direct map
// and the vmap area is not something to discover from a failing test run, and
// a `const` assertion fails in the build that introduced it. Both layouts are
// checked on every build, whichever one the build uses.
const _: () = assert!(
    LAYOUT_64.violation().is_none(),
    "LAYOUT_64 is inconsistent; the layout tests name the rule"
);
const _: () = assert!(
    LAYOUT_32.violation().is_none(),
    "LAYOUT_32 is inconsistent; the layout tests name the rule"
);

/// The layout this build uses.
#[cfg(target_pointer_width = "64")]
pub const LAYOUT: Layout = LAYOUT_64;
/// The layout this build uses.
#[cfg(target_pointer_width = "32")]
pub const LAYOUT: Layout = LAYOUT_32;

/// Base of the direct map of all physical RAM.
///
/// Physical address `p` is readable at `PHYSMAP_BASE + (p - physmap_phys)`
/// once the loader's page tables are live, and stays that way for the life of
/// the kernel. See [`BootInfo::physmap_phys`].
pub const PHYSMAP_BASE: u64 = LAYOUT.physmap_base;

/// End of the direct map, exclusive.
pub const PHYSMAP_END: u64 = LAYOUT.physmap_end;

/// Base of the kernel's dynamic virtual allocation area.
pub const KERNEL_VMAP_BASE: u64 = LAYOUT.vmap_base;

/// Size of the kernel's dynamic virtual allocation area, in bytes.
pub const KERNEL_VMAP_SIZE: u64 = LAYOUT.vmap_size;

/// Bytes at the bottom of the vmap area reserved for early boot's fixed
/// windows. See [`Layout::vmap_reserved`].
pub const KERNEL_VMAP_RESERVED: u64 = LAYOUT.vmap_reserved;

/// Where the kernel image is linked. `src/kernel/build.rs` tells the linker the
/// same number, and the loader refuses a kernel linked anywhere else. It is
/// also where the image runs when it does not move; when it does (KASLR), it
/// runs at one of [`Layout::kernel_slots`] above this.
pub const KERNEL_VIRT_BASE: u64 = LAYOUT.kernel_base;

/// The lowest kernel address.
pub const KERNEL_HALF_BASE: u64 = LAYOUT.kernel_half;

/// Highest user virtual address, exclusive.
pub const USER_VIRT_END: u64 = LAYOUT.user_end;

/// The page size every architecture is configured for.
pub const PAGE_SIZE: u64 = 4096;

/// The alignment of the direct map's physical origin.
///
/// Two mebibytes, so the loader can map it with blocks rather than pages on
/// every architecture: a direct map whose first byte is halfway through a
/// block is 512 page descriptors where there should have been one.
pub const PHYSMAP_ALIGN: u64 = 2 * 1024 * 1024;

/// The stack the loader hands the kernel, in bytes. Replaced by a guard-paged
/// per-CPU stack as soon as the kernel can allocate one.
pub const BOOT_STACK_SIZE: u64 = 64 * 1024;

/// True if `addr` is in the part of the address space reserved for the kernel.
#[must_use]
pub const fn is_kernel_address(addr: u64) -> bool {
    addr >= KERNEL_HALF_BASE
}

/// True if `addr` is a valid user virtual address.
#[must_use]
pub const fn is_user_address(addr: u64) -> bool {
    addr < USER_VIRT_END
}

/// The physical origin a direct map should have, given the lowest physical
/// address of RAM: that address, rounded down to [`PHYSMAP_ALIGN`].
///
/// Zero on x86-64, where RAM starts at zero. A gibibyte on QEMU's Arm `virt`
/// machines, whose first gibibyte is flash and device registers — which a
/// direct map from zero would have covered, as cacheable memory, at the cost
/// of a quarter of a 32-bit kernel's half.
#[must_use]
pub const fn physmap_origin(lowest_ram: u64) -> u64 {
    lowest_ram & !(PHYSMAP_ALIGN - 1)
}

/// The direct-map address of `phys`, for a direct map whose physical origin
/// is `physmap_phys`.
///
/// `phys` must not be below the origin; nothing below it is mapped, and the
/// subtraction says so by overflowing rather than by wrapping into an address
/// that happens to be mapped.
#[must_use]
pub const fn direct_map_address(physmap_phys: u64, phys: u64) -> u64 {
    PHYSMAP_BASE + (phys - physmap_phys)
}

/// The runs of physical memory a direct map spanning `len` bytes from
/// `origin` actually translates: everything the memory map describes, other
/// than device registers, merged where regions touch, in ascending order.
///
/// The span runs from the lowest RAM to the highest, and between RAM banks it
/// can hold device registers, or nothing at all. A mapping over those is
/// normal, cacheable memory to the processor, which on the Arm pair licenses
/// it to read ahead and speculate into registers that have side effects. So
/// what the memory map does not describe as memory stays unmapped.
///
/// Reserved regions are memory, and kept: firmware tables live in them on
/// some machines, and the kernel reads ACPI through the direct map.
///
/// `regions` may be in any order — firmware's map is not sorted until the
/// loader copies it — which is why it is cloned and rescanned rather than
/// walked once. A map is a few hundred regions at most.
pub fn direct_map_runs<I>(regions: I, origin: u64, len: u64) -> DirectMapRuns<I>
where
    I: Iterator<Item = MemRegion> + Clone,
{
    DirectMapRuns {
        regions,
        cursor: origin,
        end: origin.saturating_add(len),
    }
}

/// Bytes of an ACPI 2.0 root system description pointer, the longer of its
/// two forms; the 1.0 form is the first 20 of them.
pub const RSDP_LEN: u64 = 36;

/// The whole pages holding an RSDP at `rsdp`, as a region the direct map has
/// to translate whatever the memory map says, or `None` when there is none.
///
/// The RSDP is the one firmware table the kernel is handed a pointer to
/// rather than finds inside another table, and on a PC it is often in the
/// legacy BIOS area between `0xE0000` and `0xFFFFF`, which a UEFI memory map
/// is free not to describe. The tables it leads to are required to live in
/// described ACPI or reserved memory; it is not. Chained onto the regions
/// given to [`direct_map_runs`], it is mapped whether or not firmware
/// described it, and merged with the region around it when firmware did.
#[must_use]
pub fn rsdp_region(rsdp: u64) -> Option<MemRegion> {
    if rsdp == 0 {
        return None;
    }
    let base = rsdp & !(PAGE_SIZE - 1);
    let end = rsdp
        .checked_add(RSDP_LEN)?
        .checked_next_multiple_of(PAGE_SIZE)?;
    Some(MemRegion {
        base,
        len: end - base,
        kind: MemKind::Reserved,
        reserved: 0,
    })
}

/// The iterator [`direct_map_runs`] returns, yielding `(base, len)` pairs.
#[derive(Clone, Debug)]
pub struct DirectMapRuns<I> {
    regions: I,
    /// Everything below this has been yielded or skipped.
    cursor: u64,
    /// One past the last address the direct map spans.
    end: u64,
}

impl<I> Iterator for DirectMapRuns<I>
where
    I: Iterator<Item = MemRegion> + Clone,
{
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        let cursor = self.cursor;
        let mapped = |region: &MemRegion| region.kind != MemKind::Mmio && region.len != 0;

        // The lowest described address at or above the cursor. A region that
        // straddles the cursor starts the run at the cursor itself.
        let start = self
            .regions
            .clone()
            .filter(|region| mapped(region) && region.end() > cursor)
            .map(|region| region.base.max(cursor))
            .min()?;
        if start >= self.end {
            self.cursor = self.end;
            return None;
        }

        // Grow the run while some region begins inside it, or exactly at its
        // end, and reaches further.
        let mut run_end = start;
        while let Some(further) = self
            .regions
            .clone()
            .filter(|region| mapped(region) && region.base <= run_end && region.end() > run_end)
            .map(|region| region.end())
            .max()
        {
            run_end = further;
        }

        let run_end = run_end.min(self.end);
        self.cursor = run_end;
        Some((start, run_end - start))
    }
}

/// The physical span of a kernel image's text and read-only data, from its
/// loadable segments as `(first byte, one past the last, writable)`: the
/// smallest span holding every segment that is not writable, or `None` if
/// there is none.
///
/// The direct map aliases every byte of RAM, the image's own included. An
/// alias of the text that can be written is a way to change the kernel's code
/// that never touches the image mapping, whose W^X then protects nothing; so
/// both loaders map this span of the direct map read only. The image's data
/// and `.bss` stay writable there: they are writable in the image mapping
/// anyway, and the kernel's early page tables are in `.bss` and are written
/// through the direct map.
///
/// # Errors
///
/// A writable segment inside the span. The span would have to be cut around
/// it, and the link script's order — text, read-only data, then data — says
/// that cannot happen; a script that broke the order would otherwise get a
/// read-only direct map over data the kernel writes through it.
pub fn read_only_span<I>(segments: I) -> Result<Option<(u64, u64)>, &'static str>
where
    I: Iterator<Item = (u64, u64, bool)> + Clone,
{
    let sealed = segments.clone().filter(|&(_, _, writable)| !writable);
    let base = sealed.clone().map(|(base, _, _)| base).min();
    let end = sealed.map(|(_, end, _)| end).max();
    let (Some(base), Some(end)) = (base, end) else {
        return Ok(None);
    };
    if segments
        .filter(|&(_, _, writable)| writable)
        .any(|(from, to, _)| from < end && to > base)
    {
        return Err("a writable kernel segment lies between the read-only ones");
    }
    Ok(Some((base, end)))
}

/// One run of the direct map, as [`direct_map_runs`] yields it, cut around
/// `sealed`, the span [`read_only_span`] found: the part below it, the part
/// inside it and the part above it, each as `(base, len, read_only)` and each
/// `None` where it is empty.
///
/// Three pieces rather than a list, because a run and a span have at most
/// that many; the mapper picks the largest block that fits each piece, so the
/// cost of the cut is a table or two of 4 KiB pages at its two edges.
#[must_use]
pub fn split_run(
    (base, len): (u64, u64),
    sealed: Option<(u64, u64)>,
) -> [Option<(u64, u64, bool)>; 3] {
    let end = base.saturating_add(len);
    let piece =
        |from: u64, to: u64, read_only: bool| (from < to).then_some((from, to - from, read_only));
    let Some((seal_base, seal_end)) = sealed else {
        return [piece(base, end, false), None, None];
    };
    let inside_base = seal_base.clamp(base, end);
    let inside_end = seal_end.clamp(inside_base, end);
    [
        piece(base, inside_base, false),
        piece(inside_base, inside_end, true),
        piece(inside_end, end, false),
    ]
}

// ---------------------------------------------------------------------------
// The loader's identity map
// ---------------------------------------------------------------------------

/// Which tree the loader's transient identity mapping of its own code lives in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdentityTree {
    /// One of the loader's own, which the kernel drops whole: the low entries
    /// of the single tree on x86-64, the `TTBR0` regime on the two Arm
    /// architectures.
    Separate,
    /// The kernel's own. The addresses to be mapped are in the kernel's half,
    /// which is the only half that tree translates, so there is nowhere else
    /// for the mapping to go — and the kernel has to unmap it rather than
    /// abandon a table.
    Kernel,
    /// Neither. The loader's image is where the kernel's half is already
    /// spoken for, so what gets mapped is not the loader at all but a page
    /// holding a copy of the switch, placed in RAM below the split and mapped
    /// in the separate tree. The plan's `base` and `len` are then the range
    /// that page has to come from, not a mapping; the loader picks the page.
    Trampoline,
}

/// Where the loader may map its own code at its own address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IdentityPlan {
    /// The tree the mapping belongs in.
    pub tree: IdentityTree,
    /// Lowest address mapped. Physical and virtual are the same number here,
    /// which is the whole point of the mapping.
    pub base: u64,
    /// Bytes from there.
    pub len: u64,
}

/// Where a loader put the two kernel regions that move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    /// The direct map's base.
    pub physmap_base: u64,
    /// The kernel image's base.
    pub kernel_base: u64,
}

impl Placement {
    /// Both at their fixed addresses.
    #[must_use]
    pub const fn fixed(layout: &Layout) -> Placement {
        Placement {
            physmap_base: layout.physmap_base,
            kernel_base: layout.kernel_base,
        }
    }
}

/// True if `a_start..a_end` and `b_start..b_end` share an address.
const fn overlaps(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

impl Layout {
    /// Decide where the loader's identity map goes on a machine with this
    /// memory.
    ///
    /// The instruction after the one that installs the loader's tables is
    /// fetched from the address it would have been fetched from before them,
    /// so something has to map the loader's own code at its own address for
    /// the length of the switch. Which tree can hold that mapping depends on
    /// where the machine keeps its RAM:
    ///
    /// * **All of it below the split**, which is every machine QEMU boots
    ///   here. The loader maps all of RAM in a tree of its own: a handful of
    ///   tables, and it keeps the identity map a separate thing that is
    ///   dropped whole.
    /// * **RAM above the split**, which is the STM32MP157 with its DDR at
    ///   3 GiB. Those addresses are the kernel's half, so only the kernel's
    ///   tree translates them, and what gets mapped is the loader's image
    ///   alone rather than all of RAM.
    ///
    /// * **The loader's image under a kernel region**, on a machine that also
    ///   has RAM below the split — QEMU's `virt` with 2 GiB, whose direct map
    ///   runs over the addresses firmware loaded the loader at. Nothing can map
    ///   the loader where it is, so a copy of the switch is run from a page
    ///   below the split instead; see [`IdentityTree::Trampoline`].
    ///
    /// `physmap_phys` and `physmap_len` are the direct map, which is also the
    /// RAM the first case maps; `loader_base` and `loader_len` are the loader
    /// image as firmware placed it; `kernel_len` is the kernel image, which is
    /// at [`Layout::kernel_base`] by construction.
    ///
    /// # Errors
    ///
    /// When the loader's image sits where the kernel's address space is
    /// already spoken for *and* the machine has no RAM below the split to put
    /// a trampoline in, naming the region it collided with. Each of those has
    /// a different answer — move the direct map, move the image, move the
    /// arena — and none of them is this function's to choose.
    pub const fn plan_identity_map(
        &self,
        physmap_phys: u64,
        physmap_len: u64,
        loader_base: u64,
        loader_len: u64,
        kernel_len: u64,
    ) -> Result<IdentityPlan, &'static str> {
        self.plan_identity_map_in(
            Placement::fixed(self),
            physmap_phys,
            physmap_len,
            loader_base,
            loader_len,
            kernel_len,
        )
    }

    /// [`Layout::plan_identity_map`], with the direct map and the kernel image
    /// where `placed` says the loader put them this boot rather than at their
    /// fixed addresses.
    ///
    /// # Errors
    ///
    /// As [`Layout::plan_identity_map`].
    pub const fn plan_identity_map_in(
        &self,
        placed: Placement,
        physmap_phys: u64,
        physmap_len: u64,
        loader_base: u64,
        loader_len: u64,
        kernel_len: u64,
    ) -> Result<IdentityPlan, &'static str> {
        // Every byte of RAM is translatable through a tree of the loader's
        // own, so map all of it there, as every machine with RAM below the
        // split has always done.
        if physmap_phys + physmap_len <= self.user_end {
            return Ok(IdentityPlan {
                tree: IdentityTree::Separate,
                base: physmap_phys,
                len: physmap_len,
            });
        }

        // Otherwise only the loader's own image is mapped, by whole pages,
        // because that is all the switch fetches through.
        let base = loader_base & !(PAGE_SIZE - 1);
        let end = (loader_base + loader_len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

        if end <= self.user_end {
            return Ok(IdentityPlan {
                tree: IdentityTree::Separate,
                base,
                len: end - base,
            });
        }

        let collision = if base < self.user_end {
            Some("firmware placed the loader's image across the split between the halves")
        } else if overlaps(
            base,
            end,
            placed.physmap_base,
            placed.physmap_base + physmap_len,
        ) {
            Some("the loader's image is where the kernel's direct map already is")
        } else if overlaps(base, end, self.vmap_base, self.vmap_base + self.vmap_size) {
            Some("the loader's image is where the kernel's mapping arena already is")
        } else if overlaps(
            base,
            end,
            placed.kernel_base,
            placed.kernel_base + kernel_len,
        ) {
            Some("the loader's image is where the kernel image itself is")
        } else {
            None
        };
        if let Some(reason) = collision {
            // RAM starts below the split, so there is a page to run the switch
            // from that the separate tree can map at its own address.
            if physmap_phys < self.user_end {
                return Ok(IdentityPlan {
                    tree: IdentityTree::Trampoline,
                    base: physmap_phys,
                    len: self.user_end - physmap_phys,
                });
            }
            return Err(reason);
        }

        Ok(IdentityPlan {
            tree: IdentityTree::Kernel,
            base,
            len: end - base,
        })
    }
}

// ---------------------------------------------------------------------------
// Machine description
// ---------------------------------------------------------------------------

/// The CPU architecture the loader booted on.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    /// 64-bit x86.
    X86_64 = 0,
    /// 64-bit Arm.
    AArch64 = 1,
    /// 32-bit Arm, ARMv7-A with the Large Physical Address Extension.
    Armv7a = 2,
}

impl Arch {
    /// The `e_machine` value an ELF image for this architecture carries.
    #[must_use]
    pub const fn elf_machine(self) -> u16 {
        match self {
            Arch::X86_64 => 62,
            Arch::AArch64 => 183,
            Arch::Armv7a => 40,
        }
    }

    /// The short name used in paths, log lines and target triples.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::AArch64 => "aarch64",
            Arch::Armv7a => "armv7a",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a region of physical address space is being used for.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemKind {
    /// Free for the kernel's frame allocator.
    Usable = 0,
    /// Firmware or hardware reserved; never hand out.
    Reserved = 1,
    /// ACPI tables, reclaimable once the kernel has parsed them.
    AcpiReclaim = 2,
    /// ACPI non-volatile storage, which must be preserved.
    AcpiNvs = 3,
    /// The loader's own code and data, reclaimable after early boot.
    Loader = 4,
    /// The loaded kernel image.
    Kernel = 5,
    /// The initial ramdisk.
    Initrd = 6,
    /// Page tables built by the loader; still live, so never reuse them.
    PageTables = 7,
    /// The stack the kernel is entered on.
    BootStack = 8,
    /// The boot info structure and the memory map array inside it.
    BootInfo = 12,
    /// The loader's copy of the flattened device tree, kept for the life of
    /// the system: stage 10 enumerates devices from it long after boot.
    DeviceTree = 13,
    /// The loader's copy of firmware's SMBIOS entry point and table, kept
    /// for the life of the system: sysfs shows it (`docs/DMI.md`).
    Smbios = 15,
    /// A linear framebuffer.
    Framebuffer = 9,
    /// A device `MMIO` aperture.
    Mmio = 10,
    /// Memory the firmware reported as bad.
    Defective = 11,
    /// The region the kernel keeps its log in across a reset ([`pstore`]):
    /// RAM, direct-mapped, never free and never reclaimed.
    Pstore = 14,
}

impl MemKind {
    /// Regions the frame allocator may claim immediately at boot.
    #[must_use]
    pub const fn is_free_at_boot(self) -> bool {
        matches!(self, MemKind::Usable)
    }

    /// Regions that become free once early boot has finished with them.
    #[must_use]
    pub const fn is_reclaimable(self) -> bool {
        matches!(self, MemKind::Loader | MemKind::AcpiReclaim)
    }

    /// Regions that are real RAM, whether or not they are free.
    ///
    /// This is what the direct map has to cover and what "total memory"
    /// counts, so it is deliberately conservative. `AcpiReclaim` and `AcpiNvs`
    /// are included — the tables the kernel has to read live there. `Reserved`
    /// is **not**, and that is the important half: UEFI reports firmware- and
    /// hardware-reserved apertures under that type, and on QEMU's q35 one of
    /// them sits at 1 TiB. Counting it as RAM makes the direct map a thousand
    /// times larger than the machine, which is not a subtle failure but is a
    /// confusing one — it appears as the page table pool running out.
    #[must_use]
    pub const fn is_ram(self) -> bool {
        !matches!(self, MemKind::Reserved | MemKind::Mmio | MemKind::Defective)
    }
}

/// One entry of the physical memory map.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MemRegion {
    /// First physical address in the region, page aligned.
    pub base: u64,
    /// Length in bytes, a multiple of [`PAGE_SIZE`].
    pub len: u64,
    /// What the region is for.
    pub kind: MemKind,
    /// Padding, so the struct has the same layout under every ABI. Zero.
    pub reserved: u32,
}

impl MemRegion {
    /// One past the last physical address in the region.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base.wrapping_add(self.len)
    }

    /// Number of [`PAGE_SIZE`] frames the region covers.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.len / PAGE_SIZE
    }

    /// True if `addr` falls inside the region.
    #[must_use]
    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.end()
    }
}

/// Pixel layout of a linear framebuffer.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelFormat {
    /// Blue, green, red, unused — the usual UEFI graphics output format.
    Bgrx8888 = 0,
    /// Red, green, blue, unused.
    Rgbx8888 = 1,
    /// Something else, or no framebuffer.
    Unknown = 2,
}

/// A linear framebuffer the firmware left us.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Framebuffer {
    /// Physical address of the first pixel, or 0 if there is no framebuffer.
    pub phys: u64,
    /// Size of the framebuffer in bytes.
    pub size: u64,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Pixels — not bytes — from the start of one scanline to the next.
    pub stride: u32,
    /// Pixel layout.
    pub format: PixelFormat,
    /// Nonzero when the framebuffer lies in memory the frame allocator will
    /// own: boot-services data, as UEFI's virtio-gpu driver allocates its
    /// framebuffer, rather than a reserved region or a device's aperture.
    /// Pixels written there land in frames the kernel has handed to someone
    /// else, and once a driver owns the device nobody sees them, so the panic
    /// screen draws only when this is 0, and the display core reads the same
    /// field before it publishes a card. The loader sets it from the final
    /// memory map with [`allocator_owns`].
    pub reclaimable: u32,
    /// Zero.
    pub reserved: u32,
}

impl Framebuffer {
    /// The value used when firmware offered no graphics output.
    pub const NONE: Framebuffer = Framebuffer {
        phys: 0,
        size: 0,
        width: 0,
        height: 0,
        stride: 0,
        format: PixelFormat::Unknown,
        reclaimable: 0,
        reserved: 0,
    };

    /// True if there is a framebuffer to draw on.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        self.phys != 0 && self.width != 0 && self.height != 0
    }

    /// True if the framebuffer lies in memory the frame allocator will own.
    #[must_use]
    pub const fn is_reclaimable(&self) -> bool {
        self.reclaimable != 0
    }
}

/// Whether the frame allocator will own any page of `len` bytes at `base`,
/// given the memory map `regions`: whether any region of a kind it claims at
/// boot or after early boot overlaps the range. A range no region describes,
/// such as a device's aperture, is not the allocator's.
pub fn allocator_owns(regions: impl IntoIterator<Item = MemRegion>, base: u64, len: u64) -> bool {
    let end = base.saturating_add(len);
    regions.into_iter().any(|region| {
        let region_end = region.base.saturating_add(region.len);
        (region.kind.is_free_at_boot() || region.kind.is_reclaimable())
            && region.base < end
            && base < region_end
    })
}

/// Mark `len` bytes at `base` as `kind` in the first `count` entries of
/// `regions`, sorted by address, splitting the one region that holds the
/// range into up to three; the new count, or `None` when no single region
/// holds the whole range or the slice has no room for the split.
///
/// The loader allocates everything as firmware's loader data, because the
/// N150's AMI firmware hangs in `exit_boot_services` once memory of an
/// OS-defined type exists; the kernel learns which loader data is its image,
/// its tables or its initramfs from this instead.
pub fn retag(
    regions: &mut [MemRegion],
    count: usize,
    base: u64,
    len: u64,
    kind: MemKind,
) -> Option<usize> {
    let end = base.checked_add(len)?;
    let at = regions
        .get(..count)?
        .iter()
        .position(|region| region.base <= base && end <= region.base.saturating_add(region.len))?;
    let holder = *regions.get(at)?;
    let holder_end = holder.base + holder.len;
    let mut pieces = [holder; 3];
    let mut made = 0;
    for (from, to, piece_kind) in [
        (holder.base, base, holder.kind),
        (base, end, kind),
        (end, holder_end, holder.kind),
    ] {
        if from < to {
            let slot = pieces.get_mut(made)?;
            *slot = MemRegion {
                base: from,
                len: to - from,
                kind: piece_kind,
                reserved: 0,
            };
            made += 1;
        }
    }
    let grown = count + made - 1;
    if grown > regions.len() {
        return None;
    }
    // Move what follows the holder up to make room, then put the pieces in.
    regions.copy_within(at + 1..count, at + made);
    regions
        .get_mut(at..at + made)?
        .copy_from_slice(pieces.get(..made)?);
    Some(grown)
}

// ---------------------------------------------------------------------------
// The hand-off structure
// ---------------------------------------------------------------------------

/// Everything the kernel learns from firmware, assembled by the loader.
///
/// Fields named for an address without a `_phys` suffix — `regions`,
/// `cmdline`, the boot stack — are *virtual* addresses valid in the page
/// tables the loader installed before jumping to the kernel, which is to say
/// inside the direct map at [`PHYSMAP_BASE`]. Fields named `_phys` are
/// physical. None of them is a pointer type, so the structure is the same
/// 360 bytes on every word width.
///
/// Read it through [`BootInfo::validate`] rather than field by field.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    /// Must equal [`BOOTINFO_MAGIC`].
    pub magic: u64,
    /// Must equal [`BOOTINFO_VERSION`].
    pub version: u32,
    /// The architecture the loader ran on.
    pub arch: Arch,

    /// Virtual address of `regions_len` [`MemRegion`]s, sorted by base and
    /// non-overlapping.
    pub regions: u64,
    /// Number of entries `regions` holds.
    pub regions_len: u64,

    /// Base of the direct physical map: one of [`Layout::physmap_slots`], a
    /// different one each boot when the loader randomises, and
    /// [`PHYSMAP_BASE`] when it does not.
    pub physmap_base: u64,
    /// The physical address that appears at `physmap_base`: the lowest RAM
    /// address, rounded down to [`PHYSMAP_ALIGN`]. Nothing below it is in the
    /// direct map.
    pub physmap_phys: u64,
    /// Bytes of physical address space the direct map spans, from
    /// `physmap_phys` up. Inside the span, only what the memory map describes
    /// as memory is mapped: device registers and undescribed holes between RAM
    /// banks are not. See [`direct_map_runs`].
    pub physmap_len: u64,

    /// Physical address the kernel image was loaded at.
    pub kernel_phys: u64,
    /// Virtual address the kernel image is mapped at: its link address
    /// ([`Kaslr::link`]) or one of [`Layout::kernel_slots`].
    pub kernel_virt: u64,
    /// Size of the kernel image in bytes, page rounded.
    pub kernel_len: u64,

    /// Physical address of the root page table for the kernel half: the
    /// x86-64 PML4, the AArch64 `TTBR1_EL1` table, or the ARMv7-A level-1
    /// table whose last two entries `TTBR1` translates through.
    pub root_table_phys: u64,
    /// Physical address of the identity-mapping `TTBR0` table on the two Arm
    /// architectures. Zero on x86-64, where one table covers both halves.
    pub ttbr0_phys: u64,
    /// Physical address of the loader's own image, where the loader had to map
    /// it at that same address inside the *kernel's* tree; 0 when the identity
    /// map was a tree of its own, which is every machine whose RAM is below the
    /// split. [`Layout::plan_identity_map`] decides which, and the kernel
    /// unmaps this where it drops the identity map.
    pub loader_alias_phys: u64,
    /// Length of that mapping in bytes, or 0 when there is none.
    pub loader_alias_len: u64,

    /// Top of the stack the kernel is entered on, 16-byte aligned.
    pub boot_stack_top: u64,
    /// Size of that stack in bytes.
    pub boot_stack_size: u64,

    /// The framebuffer, or [`Framebuffer::NONE`].
    pub framebuffer: Framebuffer,

    /// Physical address of the initial ramdisk, or 0 if there is none.
    pub initrd_phys: u64,
    /// Length of the initial ramdisk in bytes.
    pub initrd_len: u64,

    /// Physical address of the ACPI RSDP, or 0 if firmware offered none.
    pub rsdp: u64,
    /// Physical address of the loader's copy of the flattened device tree, or
    /// 0. A copy, in memory the map reports as [`MemKind::DeviceTree`], because
    /// firmware's own tends to live somewhere the kernel would reclaim.
    pub dtb: u64,
    /// Length of that copy in bytes.
    pub dtb_len: u64,
    /// Physical address of the UEFI system table. Boot services are gone by
    /// the time the kernel sees this; runtime services are still callable.
    pub uefi_system_table: u64,

    /// Virtual address of `cmdline_len` bytes of UTF-8 kernel command line,
    /// or 0.
    pub cmdline: u64,
    /// Length of the command line in bytes.
    pub cmdline_len: u64,

    /// Nanoseconds since the Unix epoch when the loader asked firmware, if
    /// [`FIRMWARE_TIME`] is set. Firmware keeps local time or UTC as the
    /// machine's owner set it; a time zone it does not know is read as UTC.
    pub firmware_time: i64,
    /// Random bytes from firmware, if [`FIRMWARE_SEED`] is set; zeros if not.
    pub firmware_seed: [u8; 32],
    /// Which of the two above firmware provided: [`FIRMWARE_TIME`] and
    /// [`FIRMWARE_SEED`].
    pub firmware_flags: u64,
    /// How many random bytes firmware gave, folded into
    /// [`BootInfo::firmware_seed`] by exclusive or, if [`FIRMWARE_SEED`] is
    /// set: 32 from `EFI_RNG_PROTOCOL`, 8 from the Pixel 7's bootloader. More
    /// than 32 are worth no more than 32, since the seed holds no more.
    pub firmware_seed_len: u64,

    /// What the loader did about layout randomisation, and with what.
    pub kaslr: Kaslr,

    /// Physical address of the region the kernel keeps its log in across a
    /// reset ([`pstore`]), inside a [`MemKind::Pstore`] entry of the map; 0
    /// when there is none.
    pub pstore_phys: u64,
    /// Its length in bytes, header page included; 0 when there is none.
    pub pstore_len: u64,

    /// Physical address of the loader's copy of firmware's SMBIOS entry point
    /// and structure table, or 0 if firmware offered none the loader accepted.
    /// The entry point is in the first [`SMBIOS_ENTRY_BYTES`], the table after
    /// them, in memory the map reports as [`MemKind::Smbios`].
    pub smbios: u64,
    /// Length of that copy in bytes: [`SMBIOS_ENTRY_BYTES`] and the table's.
    pub smbios_len: u64,
}

// The claim the module documentation makes, asserted where it can fail: a
// field whose size follows the pointer width would break it, and the loader
// and the host tests would then disagree about a layout neither of them sees.
const _: () = assert!(
    size_of::<BootInfo>() == 360,
    "BootInfo must be laid out identically on every word width"
);
const _: () = assert!(
    align_of::<BootInfo>() == 8,
    "BootInfo must be eight-byte aligned on every ABI"
);
const _: () = assert!(
    size_of::<MemRegion>() == 24,
    "MemRegion must be laid out identically on every word width"
);

/// Why a [`BootInfo`] was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BootInfoError {
    /// The magic number was wrong: this is not a `BootInfo` at all.
    BadMagic,
    /// The loader and the kernel were built from different versions of this
    /// file. Carries the version the loader wrote.
    VersionMismatch(u32),
    /// The memory map address was zero, or its length zero.
    NoMemoryMap,
    /// The direct map does not start on one of [`Layout::physmap_slots`], or
    /// runs out of its region.
    PhysmapMismatch,
    /// The direct map's physical origin is not a multiple of
    /// [`PHYSMAP_ALIGN`].
    PhysmapMisaligned,
    /// The direct map claims to cover more than its region of the address
    /// space can hold.
    PhysmapTooLarge,
    /// The command line was not valid UTF-8.
    CmdlineNotUtf8,
    /// The kernel image is not inside its region, not page aligned, or the
    /// link address the loader reports is not the one the kernel was built
    /// with.
    KernelOutsideRegion,
    /// The top of the vmap arena is not one of [`Layout::vmap_ends`].
    VmapOutsideRegion,
}

impl fmt::Display for BootInfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootInfoError::BadMagic => f.write_str("boot info magic is wrong"),
            BootInfoError::VersionMismatch(found) => {
                write!(
                    f,
                    "boot info version {found}, kernel wants {BOOTINFO_VERSION}"
                )
            }
            BootInfoError::NoMemoryMap => f.write_str("boot info carries no memory map"),
            BootInfoError::PhysmapMismatch => {
                f.write_str("direct map is not on a slot of its region")
            }
            BootInfoError::PhysmapMisaligned => {
                f.write_str("direct map's physical origin is not 2 MiB aligned")
            }
            BootInfoError::PhysmapTooLarge => {
                f.write_str("direct map is larger than its region of the address space")
            }
            BootInfoError::CmdlineNotUtf8 => f.write_str("kernel command line is not UTF-8"),
            BootInfoError::KernelOutsideRegion => {
                f.write_str("kernel image is not where its region and link address allow")
            }
            BootInfoError::VmapOutsideRegion => {
                f.write_str("vmap arena's top is not one the layout allows")
            }
        }
    }
}

impl BootInfo {
    /// Check the hand-off and produce a view whose accessors are safe.
    ///
    /// This is the only place the addresses in it are dereferenced, so it is
    /// the only place that has to be reasoned about.
    ///
    /// # Safety
    ///
    /// `self.regions` must be the address of `self.regions_len` initialised
    /// [`MemRegion`]s, and `self.cmdline` of `self.cmdline_len` initialised
    /// bytes, both mapped and immutable for as long as the returned
    /// [`BootView`] lives. The loader guarantees this by construction; nothing
    /// else should call it.
    pub unsafe fn validate(&self) -> Result<BootView<'_>, BootInfoError> {
        if self.magic != BOOTINFO_MAGIC {
            return Err(BootInfoError::BadMagic);
        }
        if self.version != BOOTINFO_VERSION {
            return Err(BootInfoError::VersionMismatch(self.version));
        }
        if self.regions == 0 || self.regions_len == 0 {
            return Err(BootInfoError::NoMemoryMap);
        }
        if !self.physmap_phys.is_multiple_of(PHYSMAP_ALIGN) {
            return Err(BootInfoError::PhysmapMisaligned);
        }
        if self.physmap_len > PHYSMAP_END - PHYSMAP_BASE {
            return Err(BootInfoError::PhysmapTooLarge);
        }
        if self.physmap_base < PHYSMAP_BASE
            || !(self.physmap_base - PHYSMAP_BASE).is_multiple_of(LAYOUT.physmap_granule())
            || self.physmap_base > PHYSMAP_END - self.physmap_len
        {
            return Err(BootInfoError::PhysmapMismatch);
        }
        if self.kaslr.link != KERNEL_VIRT_BASE
            || self.kernel_virt < KERNEL_VIRT_BASE
            || !self.kernel_virt.is_multiple_of(PAGE_SIZE)
            || LAYOUT
                .kernel_ceiling()
                .checked_sub(self.kernel_virt)
                .is_none_or(|room| self.kernel_len > room)
        {
            return Err(BootInfoError::KernelOutsideRegion);
        }
        if !LAYOUT.is_vmap_end(self.kaslr.vmap_end) {
            return Err(BootInfoError::VmapOutsideRegion);
        }

        // The addresses were exposed by whoever wrote them — the loader, or a
        // test — and are taken back with the provenance that exposure left.
        let regions = core::ptr::with_exposed_provenance::<MemRegion>(self.regions as usize);
        // SAFETY: the caller's contract is exactly that this address and length
        // describe an initialised, immutable array.
        let regions = unsafe { core::slice::from_raw_parts(regions, self.regions_len as usize) };

        let cmdline = if self.cmdline == 0 || self.cmdline_len == 0 {
            ""
        } else {
            let bytes = core::ptr::with_exposed_provenance::<u8>(self.cmdline as usize);
            // SAFETY: as above, for the command line bytes.
            let bytes = unsafe { core::slice::from_raw_parts(bytes, self.cmdline_len as usize) };
            core::str::from_utf8(bytes).map_err(|_| BootInfoError::CmdlineNotUtf8)?
        };

        Ok(BootView {
            info: self,
            regions,
            cmdline,
        })
    }
}

/// A validated [`BootInfo`]: same data, safe to read.
#[derive(Clone, Copy, Debug)]
pub struct BootView<'a> {
    info: &'a BootInfo,
    regions: &'a [MemRegion],
    cmdline: &'a str,
}

impl<'a> BootView<'a> {
    /// The raw hand-off structure, for the fields without an accessor.
    #[must_use]
    pub const fn raw(&self) -> &'a BootInfo {
        self.info
    }

    /// The physical memory map, sorted by base address.
    #[must_use]
    pub const fn regions(&self) -> &'a [MemRegion] {
        self.regions
    }

    /// The kernel command line, possibly empty.
    #[must_use]
    pub const fn cmdline(&self) -> &'a str {
        self.cmdline
    }

    /// The architecture the loader ran on.
    #[must_use]
    pub const fn arch(&self) -> Arch {
        self.info.arch
    }

    /// Total bytes of RAM the firmware reported, free or not.
    #[must_use]
    pub fn total_ram(&self) -> u64 {
        self.regions
            .iter()
            .filter(|region| region.kind.is_ram())
            .map(|region| region.len)
            .sum()
    }

    /// Bytes the frame allocator can claim at boot.
    #[must_use]
    pub fn usable_ram(&self) -> u64 {
        self.regions
            .iter()
            .filter(|region| region.kind.is_free_at_boot())
            .map(|region| region.len)
            .sum()
    }

    /// One past the highest physical address of real RAM.
    ///
    /// The direct map has to cover at least this much.
    #[must_use]
    pub fn max_ram_address(&self) -> u64 {
        self.regions
            .iter()
            .filter(|region| region.kind.is_ram())
            .map(MemRegion::end)
            .max()
            .unwrap_or(0)
    }

    /// One past the highest physical address the direct map covers.
    ///
    /// Below [`BootView::max_ram_address`] only on a 32-bit machine with more
    /// RAM than its direct map can hold, where the difference is memory the
    /// kernel cannot reach and does not pretend to.
    #[must_use]
    pub const fn physmap_limit(&self) -> u64 {
        self.info.physmap_phys + self.info.physmap_len
    }

    /// Where the frame allocator's `bytes`-long per-frame array can go: the
    /// largest usable run of RAM *inside the direct map* that holds it.
    ///
    /// The kernel zeroes the array through the direct map before there is any
    /// other way to reach physical memory, so a host region the direct map
    /// does not cover is not a smaller mistake than no region at all. On a
    /// 32-bit machine with more RAM than its direct map, the longest usable
    /// region can lie wholly above the limit, and zeroing "its" direct-map
    /// address lands on whatever is mapped past the end — the kernel image.
    ///
    /// Each usable region is clipped to the direct map first, page-rounded
    /// inwards, and only then compared. `None` if no clipped region is long
    /// enough.
    #[must_use]
    pub fn page_array_host(&self, bytes: u64) -> Option<MemRegion> {
        let floor = self.info.physmap_phys.next_multiple_of(PAGE_SIZE);
        let limit = self.physmap_limit() & !(PAGE_SIZE - 1);
        self.regions
            .iter()
            .filter(|region| region.kind.is_free_at_boot())
            .filter_map(|region| {
                let base = region.base.max(floor).next_multiple_of(PAGE_SIZE);
                let end = region.end().min(limit) & !(PAGE_SIZE - 1);
                // `then`, not `then_some`: the length is only a number once
                // the clipped region is known not to be empty.
                (end > base).then(|| MemRegion {
                    base,
                    len: end - base,
                    ..*region
                })
            })
            .filter(|region| region.len >= bytes)
            .max_by_key(|region| region.len)
    }

    /// The region containing `phys`, if the map describes one.
    #[must_use]
    pub fn region_of(&self, phys: u64) -> Option<&'a MemRegion> {
        self.regions.iter().find(|region| region.contains(phys))
    }

    /// The initial ramdisk as a physical address and length, if present.
    #[must_use]
    pub const fn initrd(&self) -> Option<(u64, u64)> {
        if self.info.initrd_phys == 0 || self.info.initrd_len == 0 {
            None
        } else {
            Some((self.info.initrd_phys, self.info.initrd_len))
        }
    }

    /// The loader's SMBIOS copy as a physical address and length, if present.
    #[must_use]
    pub const fn smbios(&self) -> Option<(u64, u64)> {
        if self.info.smbios == 0 || self.info.smbios_len == 0 {
            None
        } else {
            Some((self.info.smbios, self.info.smbios_len))
        }
    }

    /// The device tree as a physical address and length, if present.
    #[must_use]
    pub const fn device_tree(&self) -> Option<(u64, u64)> {
        if self.info.dtb == 0 || self.info.dtb_len == 0 {
            None
        } else {
            Some((self.info.dtb, self.info.dtb_len))
        }
    }

    /// The loader's identity mapping of its own image inside the kernel's own
    /// tree, if the machine needed one: an address that is both the virtual and
    /// the physical one, and a length.
    ///
    /// Present only where RAM is above the split between the two halves of the
    /// address space; see [`Layout::plan_identity_map`].
    #[must_use]
    pub const fn loader_alias(&self) -> Option<(u64, u64)> {
        if self.info.loader_alias_phys == 0 || self.info.loader_alias_len == 0 {
            None
        } else {
            Some((self.info.loader_alias_phys, self.info.loader_alias_len))
        }
    }

    /// Look up a `key=value` option on the kernel command line.
    ///
    /// Options are whitespace separated; the first match wins.
    #[must_use]
    pub fn option(&self, key: &str) -> Option<&'a str> {
        option_in(self.cmdline, key)
    }

    /// The region the kernel keeps its log in across a reset, as
    /// `(physical address, length)`, or `None` when the length is 0: an
    /// address with no length is no region, and is ignored. What the region
    /// is, the kernel checks for itself before it writes there.
    #[must_use]
    pub const fn pstore(&self) -> Option<(u64, u64)> {
        if self.info.pstore_len == 0 {
            None
        } else {
            Some((self.info.pstore_phys, self.info.pstore_len))
        }
    }

    /// True if the command line carries `flag` as a bare word.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        flag_in(self.cmdline, name)
    }
}

/// The kernel command line a loader read from a file, as the kernel will be
/// handed it: surrounding whitespace dropped, so the newline an editor adds is
/// not part of the last option.
///
/// # Errors
///
/// A file that is not UTF-8, which [`BootInfo::validate`] would refuse, or
/// whose text is longer than `capacity` bytes, the room the loader has for it.
pub fn command_line_from_file(bytes: &[u8], capacity: usize) -> Result<&str, &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "it is not UTF-8")?;
    let text = text.trim();
    if text.len() > capacity {
        return Err("it is longer than the room the boot info has for it");
    }
    Ok(text)
}

/// Look up a `key=value` option in a command line the caller already has.
///
/// The same grammar as [`BootView::option`], against a string from anywhere.
/// It exists because the loader does not fill the command line in on every
/// machine, while the Arm boards carry one regardless: U-Boot writes what it
/// was told to pass into the device tree's `/chosen/bootargs`, and a kernel
/// that already parsed the tree can read it from there. One grammar for both
/// sources means an option means the same thing whichever way it arrived.
#[must_use]
pub fn option_in<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|word| {
        let (name, value) = word.split_once('=')?;
        (name == key).then_some(value)
    })
}

/// A size as Linux's `memparse` reads one from the command line: a decimal
/// number, or hexadecimal after `0x`, with an optional `K`, `M` or `G`
/// suffix in either case, each a power of 1024. `None` for anything else or
/// a size that does not fit.
#[must_use]
pub fn size_in(value: &str) -> Option<u64> {
    let (digits, shift) = match value.as_bytes().last()? {
        b'k' | b'K' => (value.get(..value.len() - 1)?, 10),
        b'm' | b'M' => (value.get(..value.len() - 1)?, 20),
        b'g' | b'G' => (value.get(..value.len() - 1)?, 30),
        _ => (value, 0),
    };
    let number = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u64>().ok()?,
    };
    number.checked_mul(1u64 << shift)
}

/// True if `cmdline` carries `name` as a bare word.
///
/// The companion to [`option_in`], and [`BootView::flag`]'s grammar.
#[must_use]
pub fn flag_in(cmdline: &str, name: &str) -> bool {
    cmdline.split_whitespace().any(|word| word == name)
}

/// The signature the loader calls.
///
/// Declared here so that a mismatch between the two programs is a type error on
/// both sides rather than a triple fault with nothing on the serial port.
pub type KernelEntry = extern "C" fn(bootinfo: *const BootInfo) -> !;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_reads_as_memparse_reads_it() {
        assert_eq!(size_in("4096"), Some(4096));
        assert_eq!(size_in("1M"), Some(1 << 20));
        assert_eq!(size_in("256k"), Some(256 << 10));
        assert_eq!(size_in("2G"), Some(2 << 30));
        assert_eq!(size_in("0x10000"), Some(0x10000));
        for bad in ["", "M", "1T", "x", "-1", "0xzz", "99999999999999999999"] {
            assert_eq!(size_in(bad), None, "{bad}");
        }
        assert_eq!(size_in("18446744073709551615K"), None);
    }

    fn regions() -> [MemRegion; 4] {
        [
            MemRegion {
                base: 0,
                len: 0x1000,
                kind: MemKind::Reserved,
                reserved: 0,
            },
            MemRegion {
                base: 0x1000,
                len: 0x9F000,
                kind: MemKind::Usable,
                reserved: 0,
            },
            MemRegion {
                base: 0x10_0000,
                len: 0x3F00_0000,
                kind: MemKind::Usable,
                reserved: 0,
            },
            MemRegion {
                base: 0xFEC0_0000,
                len: 0x1000,
                kind: MemKind::Mmio,
                reserved: 0,
            },
        ]
    }

    fn boot_info(map: &[MemRegion], cmdline: &str) -> BootInfo {
        BootInfo {
            magic: BOOTINFO_MAGIC,
            version: BOOTINFO_VERSION,
            arch: Arch::X86_64,
            regions: map.as_ptr().expose_provenance() as u64,
            regions_len: map.len() as u64,
            physmap_base: PHYSMAP_BASE,
            physmap_phys: 0,
            physmap_len: 0x1_0000_0000,
            kernel_phys: 0x20_0000,
            kernel_virt: KERNEL_VIRT_BASE,
            kernel_len: 0x10_0000,
            root_table_phys: 0x1000,
            ttbr0_phys: 0,
            loader_alias_phys: 0,
            loader_alias_len: 0,
            boot_stack_top: KERNEL_VIRT_BASE,
            boot_stack_size: BOOT_STACK_SIZE,
            framebuffer: Framebuffer::NONE,
            initrd_phys: 0,
            initrd_len: 0,
            rsdp: 0,
            dtb: 0,
            dtb_len: 0,
            uefi_system_table: 0,
            cmdline: cmdline.as_ptr().expose_provenance() as u64,
            cmdline_len: cmdline.len() as u64,
            firmware_time: 0,
            firmware_seed: [0; 32],
            firmware_flags: 0,
            firmware_seed_len: 0,
            kaslr: Kaslr::fixed(&LAYOUT, KASLR_DECLINED),
            pstore_phys: 0,
            pstore_len: 0,
            smbios: 0,
            smbios_len: 0,
        }
    }

    #[test]
    fn firmware_calendar_times_become_unix_nanoseconds() {
        const NANOS: i64 = 1_000_000_000;
        assert_eq!(unix_nanos(1970, 1, 1, 0, 0, 0, 0, 0), Some(0));
        assert_eq!(unix_nanos(1970, 1, 1, 0, 0, 1, 5, 0), Some(NANOS + 5));
        // `date -u -d 2026-09-16T21:50:45 +%s`.
        assert_eq!(
            unix_nanos(2026, 9, 16, 21, 50, 45, 0, 0),
            Some(1_789_595_445 * NANOS)
        );
        // The last day of a leap February, and the next morning.
        assert_eq!(
            unix_nanos(2024, 2, 29, 12, 0, 0, 0, 0),
            Some(1_709_208_000 * NANOS)
        );
        assert_eq!(
            unix_nanos(2024, 3, 1, 0, 0, 0, 0, 0),
            Some(1_709_251_200 * NANOS)
        );
        // An hour east of UTC is an hour earlier in UTC.
        assert_eq!(
            unix_nanos(2026, 9, 16, 22, 50, 45, 0, 60),
            Some(1_789_595_445 * NANOS)
        );
        // Before the epoch, and at the far end of the range.
        assert_eq!(unix_nanos(1969, 12, 31, 23, 59, 59, 0, 0), Some(-NANOS));
        assert_eq!(
            unix_nanos(2262, 1, 1, 0, 0, 0, 0, 0),
            Some(9_214_646_400 * NANOS)
        );
        assert_eq!(unix_nanos(2263, 1, 1, 0, 0, 0, 0, 0), None);
        for (year, month, day) in [
            (2023, 2, 29),
            (1900, 2, 29),
            (2026, 13, 1),
            (2026, 4, 31),
            (2026, 1, 0),
        ] {
            assert_eq!(
                unix_nanos(year, month, day, 0, 0, 0, 0, 0),
                None,
                "{year}-{month}-{day}"
            );
        }
        assert_eq!(unix_nanos(2026, 1, 1, 24, 0, 0, 0, 0), None);
        assert_eq!(unix_nanos(2026, 1, 1, 0, 0, 0, 1_000_000_000, 0), None);
        assert_eq!(
            unix_nanos(2000, 2, 29, 0, 0, 0, 0, 0),
            Some(951_782_400 * NANOS)
        );
    }

    #[test]
    fn validates_a_well_formed_handoff() {
        let map = regions();
        let info = boot_info(&map, "console=ttyS0 quiet");
        // SAFETY: `map` and the command line outlive the view, and the
        // structure was built by `boot_info` above, so its addresses
        // describe exactly what it says they do.
        let view = unsafe { info.validate() }.unwrap();

        assert_eq!(
            view.regions().len(),
            4,
            "every region should survive validation"
        );
        assert_eq!(view.arch(), Arch::X86_64);
        assert_eq!(view.cmdline(), "console=ttyS0 quiet");
        assert_eq!(view.device_tree(), None);
    }

    #[test]
    fn rejects_a_mismatched_version() {
        let map = regions();
        let mut info = boot_info(&map, "");
        info.version = BOOTINFO_VERSION + 1;
        assert_eq!(
            // SAFETY: `map` and the command line outlive the view, and the
            // structure was built by `boot_info` above, so its addresses
            // describe exactly what it says they do.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::VersionMismatch(BOOTINFO_VERSION + 1),
            "a loader from another build must not be trusted"
        );
    }

    #[test]
    fn rejects_the_version_before_the_pstore_fields() {
        let map = regions();
        let mut info = boot_info(&map, "");
        info.version = 7;
        assert_eq!(
            // SAFETY: as in `rejects_a_mismatched_version`.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::VersionMismatch(7),
            "a version 7 loader writes no pstore fields, and must not be read as one that did"
        );
    }

    #[test]
    fn rejects_the_version_before_the_smbios_fields() {
        let map = regions();
        let mut info = boot_info(&map, "");
        info.version = 8;
        assert_eq!(
            // SAFETY: as in `rejects_a_mismatched_version`.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::VersionMismatch(8),
            "a version 8 loader writes no SMBIOS fields, and must not be read as one that did"
        );
        assert_eq!(BOOTINFO_VERSION, 9, "version 9 carries the SMBIOS fields");
    }

    #[test]
    fn an_smbios_copy_needs_both_its_address_and_its_length() {
        let map = regions();
        let mut info = boot_info(&map, "");
        // SAFETY: as in `rejects_a_mismatched_version`.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.smbios(), None, "zeros, no copy");
        info.smbios = 0x200_0000;
        // SAFETY: as above.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.smbios(), None, "no length, no copy");
        info.smbios_len = 0x1000;
        // SAFETY: as above.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.smbios(), Some((0x200_0000, 0x1000)), "both given");
    }

    #[test]
    fn a_pstore_address_with_no_length_is_ignored() {
        let map = regions();
        let mut info = boot_info(&map, "");
        info.pstore_phys = 0x100_0000;
        // SAFETY: as in `rejects_a_mismatched_version`.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.pstore(), None, "no length, no region");
        info.pstore_len = 0x10_1000;
        // SAFETY: as above.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.pstore(), Some((0x100_0000, 0x10_1000)), "both given");
    }

    #[test]
    fn only_usable_is_free_and_only_loader_and_acpi_reclaim_are_reclaimed() {
        let every = [
            MemKind::Usable,
            MemKind::Reserved,
            MemKind::AcpiReclaim,
            MemKind::AcpiNvs,
            MemKind::Loader,
            MemKind::Kernel,
            MemKind::Initrd,
            MemKind::PageTables,
            MemKind::BootStack,
            MemKind::BootInfo,
            MemKind::DeviceTree,
            MemKind::Framebuffer,
            MemKind::Mmio,
            MemKind::Defective,
            MemKind::Pstore,
        ];
        for kind in every {
            assert_eq!(
                kind.is_free_at_boot(),
                kind == MemKind::Usable,
                "{kind:?} free at boot"
            );
            assert_eq!(
                kind.is_reclaimable(),
                matches!(kind, MemKind::Loader | MemKind::AcpiReclaim),
                "{kind:?} reclaimable"
            );
        }
    }

    #[test]
    fn pstore_is_ram_never_free_never_reclaimed() {
        assert!(MemKind::Pstore.is_ram(), "direct-mapped");
        assert!(!MemKind::Pstore.is_free_at_boot(), "not free at boot");
        assert!(!MemKind::Pstore.is_reclaimable(), "not reclaimed");
        assert_eq!(
            MemKind::Pstore as u32,
            14,
            "appended, existing values unchanged"
        );
    }

    #[test]
    fn rejects_bad_magic_before_anything_else() {
        let map = regions();
        let mut info = boot_info(&map, "");
        info.magic = 0;
        info.regions = 0;
        assert_eq!(
            // SAFETY: `map` and the command line outlive the view, and the
            // structure was built by `boot_info` above, so its addresses
            // describe exactly what it says they do.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::BadMagic,
            "magic is checked first so a wild address is never followed"
        );
    }

    #[test]
    fn rejects_a_direct_map_the_kernel_cannot_use() {
        let map = regions();

        let mut info = boot_info(&map, "");
        info.physmap_phys = 0x4000_1000;
        assert_eq!(
            // SAFETY: as in the tests above.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::PhysmapMisaligned,
            "an origin inside a block would make the direct map pages, not blocks"
        );

        let mut info = boot_info(&map, "");
        info.physmap_len = PHYSMAP_END - PHYSMAP_BASE + PHYSMAP_ALIGN;
        assert_eq!(
            // SAFETY: as in the tests above.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::PhysmapTooLarge,
            "a direct map larger than its region would run into the next one"
        );

        let mut info = boot_info(&map, "");
        info.physmap_base = KERNEL_VMAP_BASE;
        assert_eq!(
            // SAFETY: as in the tests above.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::PhysmapMismatch
        );

        let mut info = boot_info(&map, "");
        info.physmap_base = PHYSMAP_BASE + LAYOUT.physmap_granule() / 2;
        assert_eq!(
            // SAFETY: as in the tests above.
            unsafe { info.validate() }.unwrap_err(),
            BootInfoError::PhysmapMismatch,
            "a direct map between two slots is one no loader chose"
        );

        let mut info = boot_info(&map, "");
        info.physmap_base = PHYSMAP_BASE + 5 * LAYOUT.physmap_granule();
        // SAFETY: as in the tests above.
        let valid = unsafe { info.validate() }.is_ok();
        assert!(
            valid,
            "a direct map moved by whole slots is one a loader chose"
        );
    }

    #[test]
    fn rejects_a_kernel_or_arena_a_loader_could_not_have_placed() {
        let map = regions();

        let mut info = boot_info(&map, "");
        info.kernel_virt = KERNEL_VIRT_BASE + 0x2_0000;
        // SAFETY: as in the tests above.
        assert!(unsafe { info.validate() }.is_ok());

        for (virt, link) in [
            (KERNEL_VIRT_BASE - PAGE_SIZE, KERNEL_VIRT_BASE),
            (KERNEL_VIRT_BASE + 1, KERNEL_VIRT_BASE),
            (LAYOUT.kernel_ceiling(), KERNEL_VIRT_BASE),
            (KERNEL_VIRT_BASE, KERNEL_VIRT_BASE + PAGE_SIZE),
        ] {
            let mut info = boot_info(&map, "");
            info.kernel_virt = virt;
            info.kaslr.link = link;
            assert_eq!(
                // SAFETY: as in the tests above.
                unsafe { info.validate() }.unwrap_err(),
                BootInfoError::KernelOutsideRegion,
                "{virt:#x} linked at {link:#x}"
            );
        }

        let mut info = boot_info(&map, "");
        info.kaslr.vmap_end = LAYOUT.vmap_end() - LAYOUT.vmap_granule();
        // SAFETY: as in the tests above.
        assert!(unsafe { info.validate() }.is_ok());
        for end in [
            LAYOUT.vmap_end() - PAGE_SIZE,
            LAYOUT.vmap_end() + LAYOUT.vmap_granule(),
            0,
        ] {
            let mut info = boot_info(&map, "");
            info.kaslr.vmap_end = end;
            assert_eq!(
                // SAFETY: as in the tests above.
                unsafe { info.validate() }.unwrap_err(),
                BootInfoError::VmapOutsideRegion,
                "{end:#x}"
            );
        }
    }

    #[test]
    fn a_framebuffer_in_memory_the_allocator_claims_is_reclaimable() {
        let region = |base: u64, len: u64, kind| MemRegion {
            base,
            len,
            kind,
            reserved: 0,
        };
        let map = [
            region(0x4000_0000, 0x1000_0000, MemKind::Usable),
            region(0x5000_0000, 0x0100_0000, MemKind::Reserved),
            region(0x5100_0000, 0x0010_0000, MemKind::Loader),
            region(0x5200_0000, 0x0010_0000, MemKind::Kernel),
        ];
        // Boot-services data became Usable: VirtioGpuDxe's framebuffer.
        assert!(allocator_owns(map, 0x4800_0000, 0x30_0000));
        // A reserved region: ramfb's.
        assert!(!allocator_owns(map, 0x5000_0000, 0x30_0000));
        // No region at all: a BAR, as q35's VGA.
        assert!(!allocator_owns(map, 0x8000_0000, 0x100_0000));
        // Loader data is reclaimed after early boot; the kernel image is not.
        assert!(allocator_owns(map, 0x5100_0000, 0x1000));
        assert!(!allocator_owns(map, 0x5200_0000, 0x1000));
        // A range that only touches a Usable region's end is not inside it,
        // and one that straddles into it is.
        assert!(!allocator_owns(map, 0x5000_0000, 0));
        assert!(allocator_owns(map, 0x4FFF_F000, 0x2000));
        assert!(!allocator_owns([], 0x4800_0000, 0x1000));

        assert!(!Framebuffer::NONE.is_reclaimable());
    }

    #[test]
    fn sums_only_real_ram() {
        let map = regions();
        let info = boot_info(&map, "");
        // SAFETY: `map` and the command line outlive the view, and the
        // structure was built by `boot_info` above, so its addresses
        // describe exactly what it says they do.
        let view = unsafe { info.validate() }.unwrap();

        // Neither reserved memory nor MMIO counts: a reserved aperture can be
        // anywhere in the address space, and sizing the direct map from one
        // makes it enormous.
        assert_eq!(view.total_ram(), 0x9F000 + 0x3F00_0000);
        assert_eq!(view.usable_ram(), 0x9F000 + 0x3F00_0000);
        assert_eq!(view.max_ram_address(), 0x10_0000 + 0x3F00_0000);
        assert!(
            !MemKind::Reserved.is_ram(),
            "a reserved aperture at 1 TiB must not size the direct map"
        );
        assert!(
            MemKind::DeviceTree.is_ram() && !MemKind::DeviceTree.is_reclaimable(),
            "the device tree copy is RAM the kernel keeps"
        );
        assert!(
            MemKind::Smbios.is_ram() && !MemKind::Smbios.is_reclaimable(),
            "the SMBIOS copy is RAM the kernel keeps"
        );
    }

    /// A 32-bit board: 1.25 GiB of direct map from 1 GiB, and more RAM than
    /// that, with the longest usable region above the limit.
    fn beyond_the_direct_map() -> [MemRegion; 3] {
        [
            MemRegion {
                base: 0x4000_0000,
                len: 0x1000_0000,
                kind: MemKind::Usable,
                reserved: 0,
            },
            // Straddles the limit at 0x9000_0000: only the part below counts.
            MemRegion {
                base: 0x6000_0000,
                len: 0x4000_0000,
                kind: MemKind::Usable,
                reserved: 0,
            },
            // The longest region of all, and wholly out of reach.
            MemRegion {
                base: 0xC000_0000,
                len: 0x3000_0000,
                kind: MemKind::Usable,
                reserved: 0,
            },
        ]
    }

    #[test]
    fn the_page_array_is_placed_where_the_direct_map_reaches() {
        let map = beyond_the_direct_map();
        let mut info = boot_info(&map, "");
        info.physmap_phys = 0x4000_0000;
        info.physmap_len = 0x5000_0000;
        // SAFETY: as in the tests above.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.physmap_limit(), 0x9000_0000);

        let host = view.page_array_host(0x10_0000).unwrap();
        assert!(
            host.end() <= view.physmap_limit(),
            "the array host {:#x}..{:#x} runs past the direct map",
            host.base,
            host.end()
        );
        // The straddling region, clipped, is longer than the first one.
        assert_eq!((host.base, host.len), (0x6000_0000, 0x3000_0000));
    }

    #[test]
    fn no_region_inside_the_direct_map_is_reported_rather_than_guessed() {
        let map = beyond_the_direct_map();
        let mut info = boot_info(&map, "");
        info.physmap_phys = 0x4000_0000;
        info.physmap_len = 0x5000_0000;
        // SAFETY: as in the tests above.
        let view = unsafe { info.validate() }.unwrap();

        // Only the unreachable region is this large.
        assert!(view.page_array_host(0x3000_0000 + PAGE_SIZE).is_none());
        // A request the shorter reachable region can hold still falls to the
        // longest one that holds it.
        assert_eq!(
            view.page_array_host(0x3000_0000).map(|host| host.base),
            Some(0x6000_0000)
        );
    }

    #[test]
    fn a_direct_map_covering_everything_changes_nothing() {
        let map = regions();
        let info = boot_info(&map, "");
        // SAFETY: as in the tests above.
        let view = unsafe { info.validate() }.unwrap();

        let host = view.page_array_host(0x1000).unwrap();
        assert_eq!((host.base, host.len), (0x10_0000, 0x3F00_0000));
        assert!(view.page_array_host(0x3F00_0000 + PAGE_SIZE).is_none());
    }

    #[test]
    fn finds_the_region_holding_an_address() {
        let map = regions();
        let info = boot_info(&map, "");
        // SAFETY: `map` and the command line outlive the view, and the
        // structure was built by `boot_info` above, so its addresses
        // describe exactly what it says they do.
        let view = unsafe { info.validate() }.unwrap();

        assert_eq!(
            view.region_of(0x2000).map(|r| r.kind),
            Some(MemKind::Usable)
        );
        assert_eq!(
            view.region_of(0xFEC0_0000).map(|r| r.kind),
            Some(MemKind::Mmio)
        );
        assert!(
            view.region_of(0xFFFF_FFFF_0000).is_none(),
            "gaps are not regions"
        );
    }

    #[test]
    fn parses_command_line_options() {
        let map = regions();
        let info = boot_info(&map, "console=ttyS0 root=/dev/vda1 quiet sched=rt");
        // SAFETY: `map` and the command line outlive the view, and the
        // structure was built by `boot_info` above, so its addresses
        // describe exactly what it says they do.
        let view = unsafe { info.validate() }.unwrap();

        assert_eq!(view.option("console"), Some("ttyS0"));
        assert_eq!(view.option("root"), Some("/dev/vda1"));
        assert_eq!(view.option("sched"), Some("rt"));
        assert_eq!(view.option("missing"), None);
        assert!(view.flag("quiet"));
        assert!(!view.flag("verbose"));
        assert!(
            !view.flag("console"),
            "a key=value option is not a bare flag"
        );
    }

    #[test]
    fn the_free_functions_parse_what_the_view_does() {
        // The device tree path: a string the kernel got from `/chosen`, with
        // no `BootInfo` anywhere near it.
        let line = "console=stm32 nosmp root=/dev/mmcblk0p4";

        assert_eq!(option_in(line, "console"), Some("stm32"));
        assert_eq!(option_in(line, "root"), Some("/dev/mmcblk0p4"));
        assert_eq!(option_in(line, "nosmp"), None, "a bare flag has no value");
        assert_eq!(option_in(line, "absent"), None);

        assert!(flag_in(line, "nosmp"));
        assert!(!flag_in(line, "console"));
        assert!(!flag_in(line, "absent"));
    }

    #[test]
    fn an_empty_command_line_yields_nothing_rather_than_panicking() {
        // A tree with no `bootargs` hands the kernel this, and it is the
        // common case on a board that was told to pass no arguments.
        for line in ["", "   ", "\t\n"] {
            assert_eq!(option_in(line, "console"), None);
            assert!(!flag_in(line, "nosmp"));
        }
    }

    #[test]
    fn address_space_predicates_agree_with_the_layout() {
        assert!(is_user_address(0));
        assert!(is_user_address(USER_VIRT_END - 1));
        assert!(!is_user_address(USER_VIRT_END));
        assert!(is_kernel_address(KERNEL_HALF_BASE));
        assert!(is_kernel_address(PHYSMAP_BASE));
        assert!(is_kernel_address(KERNEL_VMAP_BASE));
        assert!(is_kernel_address(KERNEL_VIRT_BASE));
        assert!(!is_kernel_address(USER_VIRT_END - 1));
        // The layout constants themselves are checked at compile time, next
        // to where they are defined.
    }

    #[test]
    fn both_layouts_are_consistent() {
        assert_eq!(LAYOUT_64.violation(), None, "LAYOUT_64");
        assert_eq!(LAYOUT_32.violation(), None, "LAYOUT_32");
    }

    #[test]
    fn the_32_bit_layout_fits_four_gibibytes() {
        let layout = LAYOUT_32;
        assert_eq!(layout.user_end, 0x8000_0000, "a 2/2 split");
        assert_eq!(
            layout.kernel_half, layout.vmap_base,
            "the vmap area is the bottom of the kernel half"
        );
        assert_eq!(
            layout.physmap_size(),
            0x5000_0000,
            "1.25 GiB of direct map is the RAM a 32-bit kernel can reach"
        );
        assert!(layout.kernel_base < 1 << 32);
        // `TTBR1` translates the top 2 GiB with `TTBCR.T1SZ = 1`, so the whole
        // kernel half has to start exactly there.
        assert_eq!(layout.kernel_half, 1 << 31);
    }

    #[test]
    fn a_layout_that_overlaps_is_named_as_such() {
        let overlapping = Layout {
            physmap_end: LAYOUT_32.kernel_base + PAGE_SIZE,
            ..LAYOUT_32
        };
        assert_eq!(
            overlapping.violation(),
            Some("the kernel image must be above everything else")
        );

        let inverted = Layout {
            vmap_base: LAYOUT_32.physmap_base,
            ..LAYOUT_32
        };
        assert_eq!(
            inverted.violation(),
            Some("the direct map must not overlap the vmap area")
        );
    }

    #[test]
    fn the_direct_map_starts_at_the_lowest_ram_rounded_down() {
        assert_eq!(physmap_origin(0), 0, "RAM at zero, as on x86-64");
        assert_eq!(physmap_origin(0x4000_0000), 0x4000_0000);
        assert_eq!(physmap_origin(0x4012_3000), 0x4000_0000);
        assert_eq!(
            direct_map_address(0x4000_0000, 0x4000_2000),
            PHYSMAP_BASE + 0x2000
        );
        assert_eq!(direct_map_address(0, 0x2000), PHYSMAP_BASE + 0x2000);
    }

    #[test]
    fn region_arithmetic_is_in_frames() {
        let region = MemRegion {
            base: 0x1000,
            len: 0x3000,
            kind: MemKind::Usable,
            reserved: 0,
        };
        assert_eq!(region.end(), 0x4000);
        assert_eq!(region.frames(), 3);
        assert!(region.contains(0x1000));
        assert!(region.contains(0x3FFF));
        assert!(!region.contains(0x4000), "end is exclusive");
        assert!(!region.contains(0xFFF));
    }

    extern crate std;
    use std::vec::Vec;

    fn region(base: u64, len: u64, kind: MemKind) -> MemRegion {
        MemRegion {
            base,
            len,
            kind,
            reserved: 0,
        }
    }

    #[test]
    fn the_direct_map_leaves_device_registers_and_holes_unmapped() {
        // Two RAM banks with a device window and a hole between them, a
        // firmware reservation touching the second bank, and the whole map
        // out of order, as firmware is allowed to report it.
        let map = [
            region(0x6010_0000, 0x0FF0_0000, MemKind::Usable),
            region(0x5000_0000, 0x1000, MemKind::Mmio),
            region(0x4000_0000, 0x0800_0000, MemKind::Usable),
            region(0x6000_0000, 0x10_0000, MemKind::Reserved),
            region(0x4800_0000, 0x0800_0000, MemKind::Loader),
        ];
        let runs = |regions: &[MemRegion], origin, len| {
            direct_map_runs(regions.iter().copied(), origin, len).collect::<Vec<_>>()
        };

        assert_eq!(
            runs(&map, 0x4000_0000, 0x3000_0000),
            [(0x4000_0000, 0x1000_0000), (0x6000_0000, 0x1000_0000)],
            "the device window and the hole above it are not memory"
        );

        let mut reversed = map;
        reversed.reverse();
        assert_eq!(
            runs(&reversed, 0x4000_0000, 0x3000_0000),
            runs(&map, 0x4000_0000, 0x3000_0000),
            "the order firmware reports regions in changes nothing"
        );

        assert_eq!(
            runs(&map, 0x4000_0000, 0x2800_0000),
            [(0x4000_0000, 0x1000_0000), (0x6000_0000, 0x0800_0000)],
            "nothing past the end of the span is mapped"
        );
        assert_eq!(
            runs(&map, 0x4400_0000, 0x0C00_0000),
            [(0x4400_0000, 0x0C00_0000)],
            "a region straddling the origin is mapped from the origin"
        );
        assert!(runs(&[region(0x5000_0000, 0x1000, MemKind::Mmio)], 0, u64::MAX).is_empty());
    }

    #[test]
    fn the_image_text_and_rodata_are_one_span_and_data_is_outside_it() {
        // The link script's three segments: text, read-only data, data.
        let segments = [
            (0x20_0000, 0x40_0000, false),
            (0x40_0000, 0x48_0000, false),
            (0x48_0000, 0x50_0000, true),
        ];
        assert_eq!(
            read_only_span(segments.iter().copied()),
            Ok(Some((0x20_0000, 0x48_0000)))
        );
        assert_eq!(
            read_only_span([(0x48_0000, 0x50_0000, true)].iter().copied()),
            Ok(None),
            "an image with nothing read only seals nothing"
        );
        let broken = [
            (0x20_0000, 0x30_0000, false),
            (0x30_0000, 0x31_0000, true),
            (0x31_0000, 0x40_0000, false),
        ];
        assert!(
            read_only_span(broken.iter().copied()).is_err(),
            "data between text and rodata cannot be sealed with them"
        );
    }

    #[test]
    fn a_direct_map_run_is_cut_around_the_sealed_span_and_only_there() {
        let sealed = Some((0x20_3000, 0x48_0000));
        assert_eq!(
            split_run((0, 0x4000_0000), sealed),
            [
                Some((0, 0x20_3000, false)),
                Some((0x20_3000, 0x27_D000, true)),
                Some((0x48_0000, 0x4000_0000 - 0x48_0000, false)),
            ]
        );
        assert_eq!(
            split_run((0, 0x10_0000), sealed),
            [Some((0, 0x10_0000, false)), None, None],
            "a run below the span is untouched"
        );
        assert_eq!(
            split_run((0x100_0000, 0x10_0000), sealed),
            [None, None, Some((0x100_0000, 0x10_0000, false))],
            "a run above the span is untouched"
        );
        assert_eq!(
            split_run((0x30_0000, 0x10_0000), sealed),
            [None, Some((0x30_0000, 0x10_0000, true)), None],
            "a run inside the span is sealed whole"
        );
        assert_eq!(
            split_run((0x40_0000, 0x10_0000), sealed),
            [
                None,
                Some((0x40_0000, 0x8_0000, true)),
                Some((0x48_0000, 0x8_0000, false))
            ],
            "a run straddling the span's end is cut there"
        );
        assert_eq!(
            split_run((0, 0x10_0000), None),
            [Some((0, 0x10_0000, false)), None, None],
            "no span, no cut"
        );
        // Every byte of a run is in exactly one piece.
        for run in [(0, 0x4000_0000), (0x20_0000, 0x1000), (0x47_F000, 0x2000)] {
            let pieces = split_run(run, sealed);
            let total: u64 = pieces.iter().flatten().map(|&(_, len, _)| len).sum();
            assert_eq!(total, run.1);
        }
    }

    #[test]
    fn the_rsdp_is_mapped_even_where_the_memory_map_leaves_a_hole() {
        // A PC whose map describes nothing across the legacy BIOS area, where
        // firmware put the RSDP.
        let map = [
            region(0, 0xA_0000, MemKind::Usable),
            region(0x10_0000, 0x3FF0_0000, MemKind::Usable),
        ];
        let rsdp = rsdp_region(0xF_5B00).unwrap();
        assert_eq!((rsdp.base, rsdp.len), (0xF_5000, 0x1000));
        assert_eq!(
            direct_map_runs(map.iter().copied().chain(Some(rsdp)), 0, 0x4000_0000)
                .collect::<Vec<_>>(),
            [(0, 0xA_0000), (0xF_5000, 0x1000), (0x10_0000, 0x3FF0_0000)],
            "the RSDP's page is mapped and the rest of the hole is not"
        );
        assert_eq!(
            direct_map_runs(map.iter().copied(), 0, 0x4000_0000).collect::<Vec<_>>(),
            [(0, 0xA_0000), (0x10_0000, 0x3FF0_0000)],
            "without it, the hole would take the RSDP with it"
        );

        let straddling = rsdp_region(0xF_5FFC).unwrap();
        assert_eq!(
            (straddling.base, straddling.len),
            (0xF_5000, 0x2000),
            "an RSDP across a page boundary needs both pages"
        );

        let described = [region(0, 0x4000_0000, MemKind::Usable)];
        assert_eq!(
            direct_map_runs(
                described.iter().copied().chain(rsdp_region(0xF_5B00)),
                0,
                0x4000_0000
            )
            .collect::<Vec<_>>(),
            [(0, 0x4000_0000)],
            "an RSDP firmware described is merged, not mapped twice"
        );

        assert!(rsdp_region(0).is_none(), "no RSDP, nothing to map");
    }

    #[test]
    fn ram_below_the_split_maps_all_of_it_in_a_tree_of_its_own() {
        // QEMU's 32-bit `virt`: 512 MiB at 1 GiB, and the loader wherever
        // firmware put it inside that.
        let plan = LAYOUT_32
            .plan_identity_map(0x4000_0000, 0x2000_0000, 0x5D00_0000, 0x2_0000, 0x4_1000)
            .unwrap();

        assert_eq!(plan.tree, IdentityTree::Separate);
        assert_eq!(
            (plan.base, plan.len),
            (0x4000_0000, 0x2000_0000),
            "the machine that works today must keep mapping all of RAM"
        );
    }

    #[test]
    fn the_sixty_four_bit_layout_never_needs_the_kernel_tree() {
        let plan = LAYOUT_64
            .plan_identity_map(0, 0x2_0000_0000, 0x1DF5_A000, 0x4_0000, 0x4_1000)
            .unwrap();
        assert_eq!(
            plan.tree,
            IdentityTree::Separate,
            "no machine has RAM above a 128 TiB split"
        );
    }

    #[test]
    fn ram_above_the_split_maps_the_loader_in_the_kernels_tree() {
        // An STM32MP157 discovery board: 512 MiB of DDR at 3 GiB, with
        // firmware having placed the loader inside it.
        let plan = LAYOUT_32
            .plan_identity_map(0xC000_0000, 0x2000_0000, 0xDC34_5000, 0x3_0000, 0x4_1000)
            .unwrap();

        assert_eq!(plan.tree, IdentityTree::Kernel);
        assert_eq!(
            (plan.base, plan.len),
            (0xDC34_5000, 0x3_0000),
            "only the loader image is mapped, not all of RAM"
        );
    }

    #[test]
    fn a_loader_image_is_mapped_by_whole_pages() {
        let plan = LAYOUT_32
            .plan_identity_map(0xC000_0000, 0x2000_0000, 0xDC34_5678, 0x1234, 0x4_1000)
            .unwrap();

        assert_eq!(plan.base, 0xDC34_5000, "the first page holding it");
        assert_eq!(plan.len, 0x2000, "through the last page holding it");
    }

    #[test]
    fn a_loader_image_under_a_kernel_region_is_refused() {
        // A gibibyte at 3 GiB — an ED1 or an EV1 — puts the direct map's
        // virtual range over physical addresses the loader may land at.
        let over_direct_map =
            LAYOUT_32.plan_identity_map(0xC000_0000, 0x4000_0000, 0xC800_0000, 0x3_0000, 0x4_1000);
        assert_eq!(
            over_direct_map,
            Err("the loader's image is where the kernel's direct map already is")
        );

        let over_kernel_image =
            LAYOUT_32.plan_identity_map(0xC000_0000, 0x4000_0000, 0xF000_0000, 0x3_0000, 0x4_1000);
        assert_eq!(
            over_kernel_image,
            Err("the loader's image is where the kernel image itself is")
        );

        // RAM starting exactly at the split puts it over the arena instead.
        let over_arena =
            LAYOUT_32.plan_identity_map(0x8000_0000, 0x2000_0000, 0x8800_0000, 0x3_0000, 0x4_1000);
        assert_eq!(
            over_arena,
            Err("the loader's image is where the kernel's mapping arena already is")
        );
    }

    #[test]
    fn a_loader_image_under_a_kernel_region_uses_a_trampoline_when_ram_is_below_the_split() {
        // QEMU's 32-bit `virt` with 2 GiB: RAM at 0x4000_0000..0xC000_0000, a
        // direct map of 0x5000_0000 bytes over 0xA000_0000..0xF000_0000, and
        // firmware having loaded the loader near the top of RAM.
        let plan = LAYOUT_32
            .plan_identity_map(0x4000_0000, 0x5000_0000, 0xBE68_7000, 0x3_0000, 0x4_1000)
            .unwrap();

        assert_eq!(plan.tree, IdentityTree::Trampoline);
        assert_eq!(
            (plan.base, plan.len),
            (0x4000_0000, 0x4000_0000),
            "the page must come from RAM below the split"
        );
    }

    #[test]
    fn a_loader_image_across_the_split_uses_a_trampoline_when_ram_is_below_it() {
        let plan = LAYOUT_32
            .plan_identity_map(0x4000_0000, 0x5000_0000, 0x7FFF_0000, 0x3_0000, 0x4_1000)
            .unwrap();
        assert_eq!(plan.tree, IdentityTree::Trampoline);
    }

    #[test]
    fn a_loader_image_the_kernel_tree_can_hold_does_not_use_a_trampoline() {
        // RAM below the split, but the loader landed in the gap between the
        // arena and the direct map's end: the kernel-tree alias still works,
        // and a machine that booted before must not change path.
        let plan = LAYOUT_32
            .plan_identity_map(0x4000_0000, 0x5000_0000, 0xF800_0000, 0x3_0000, 0x4_1000)
            .unwrap();
        assert_eq!(plan.tree, IdentityTree::Kernel);
    }

    #[test]
    fn the_loader_alias_is_reported_only_when_there_is_one() {
        let map = regions();
        let mut info = boot_info(&map, "");
        // SAFETY: `map` and the command line outlive the view, and the
        // structure was built by `boot_info` above, so its addresses describe
        // exactly what it says they do.
        assert_eq!(unsafe { info.validate() }.unwrap().loader_alias(), None);

        info.loader_alias_phys = 0xDC34_5000;
        info.loader_alias_len = 0x3_0000;
        // SAFETY: as above.
        let view = unsafe { info.validate() }.unwrap();
        assert_eq!(view.loader_alias(), Some((0xDC34_5000, 0x3_0000)));
    }

    #[test]
    fn every_architecture_names_its_elf_machine() {
        assert_eq!(Arch::X86_64.elf_machine(), 62);
        assert_eq!(Arch::AArch64.elf_machine(), 183);
        assert_eq!(Arch::Armv7a.elf_machine(), 40);
        assert_eq!(Arch::Armv7a.name(), "armv7a");
    }

    #[test]
    fn a_command_line_file_loses_its_surrounding_whitespace() {
        assert_eq!(
            command_line_from_file(b"ferrix.onexit=reset\r\n", 64),
            Ok("ferrix.onexit=reset"),
            "an editor's CRLF is not part of the option"
        );
        assert_eq!(command_line_from_file(b"  a=1 b=2 \n\n", 64), Ok("a=1 b=2"));
        assert_eq!(
            command_line_from_file(b" \r\n\t", 64),
            Ok(""),
            "blank is empty"
        );
        assert_eq!(
            option_in(
                command_line_from_file(b"x ferrix.onexit=reset\n", 64).unwrap(),
                "ferrix.onexit"
            ),
            Some("reset")
        );
    }

    #[test]
    fn a_command_line_file_that_is_not_utf8_or_too_long_is_refused() {
        assert!(command_line_from_file(b"ferrix.onexit=\xff\n", 64).is_err());
        assert_eq!(
            command_line_from_file(b"abcd\n", 4),
            Ok("abcd"),
            "exactly the room fits"
        );
        assert!(command_line_from_file(b"abcde", 4).is_err());
    }

    fn loader_region(base: u64, len: u64, kind: MemKind) -> MemRegion {
        MemRegion {
            base,
            len,
            kind,
            reserved: 0,
        }
    }

    fn shape(regions: &[MemRegion]) -> Vec<(u64, u64, MemKind)> {
        regions.iter().map(|r| (r.base, r.len, r.kind)).collect()
    }

    #[test]
    fn retag_splits_the_holder_in_three_and_keeps_order() {
        let mut map = [loader_region(0, 0x1000, MemKind::Usable); 6];
        map[1] = loader_region(0x1000, 0x9000, MemKind::Loader);
        map[2] = loader_region(0xA000, 0x1000, MemKind::Reserved);
        let count = retag(&mut map, 3, 0x3000, 0x2000, MemKind::Kernel).unwrap();
        assert_eq!(count, 5);
        assert_eq!(
            shape(&map[..count]),
            [
                (0, 0x1000, MemKind::Usable),
                (0x1000, 0x2000, MemKind::Loader),
                (0x3000, 0x2000, MemKind::Kernel),
                (0x5000, 0x5000, MemKind::Loader),
                (0xA000, 0x1000, MemKind::Reserved),
            ]
        );
    }

    #[test]
    fn retag_at_an_edge_or_whole_makes_fewer_pieces() {
        let mut map = [loader_region(0x1000, 0x4000, MemKind::Loader); 4];
        let count = retag(&mut map, 1, 0x1000, 0x1000, MemKind::PageTables).unwrap();
        assert_eq!(
            shape(&map[..count]),
            [
                (0x1000, 0x1000, MemKind::PageTables),
                (0x2000, 0x3000, MemKind::Loader)
            ]
        );
        let count = retag(&mut map, count, 0x4000, 0x1000, MemKind::BootStack).unwrap();
        assert_eq!(count, 3);
        assert_eq!(map[2].kind, MemKind::BootStack);
        let count = retag(&mut map, count, 0x2000, 0x2000, MemKind::Initrd).unwrap();
        assert_eq!(count, 3, "a whole region changes kind without a split");
        assert_eq!(map[1].kind, MemKind::Initrd);
    }

    #[test]
    fn retag_refuses_a_range_no_one_region_holds_or_no_room() {
        let mut map = [loader_region(0x1000, 0x1000, MemKind::Loader); 2];
        map[1] = loader_region(0x2000, 0x1000, MemKind::Loader);
        assert_eq!(
            retag(&mut map, 2, 0x1800, 0x1000, MemKind::Kernel),
            None,
            "spans two"
        );
        assert_eq!(
            retag(&mut map, 2, 0x8000, 0x1000, MemKind::Kernel),
            None,
            "in none"
        );
        assert_eq!(
            retag(&mut map, 2, 0x1400, 0x400, MemKind::Kernel),
            None,
            "no room"
        );
    }
}
