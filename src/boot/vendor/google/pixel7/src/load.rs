//! Placing Ferrix and building the address space it starts in.
//!
//! The same job as `src/boot/common/uefi/src/load.rs`, on the same libraries -- the ELF
//! parser, the page table mapper and the layout `src/lib/proto/bootinfo` defines -- with
//! the device tree's map and [`Memory`]'s allocator where UEFI's were. Two
//! things differ, both because this is a phone rather than QEMU:
//!
//! * The identity map covers the loader's own image and nothing else. `src/boot/common/uefi/`
//!   maps all of RAM there; here RAM holds the secure world's carve-outs, and a
//!   cacheable mapping of those is somewhere a speculative access can raise an
//!   asynchronous abort from.
//! * Everything is written with the caches off, so every range the kernel is
//!   handed is invalidated before the switch rather than cleaned.

use core::ptr;

use ferrix_bootinfo::{
    Arch, BOOT_STACK_SIZE, BOOTINFO_MAGIC, BOOTINFO_VERSION, BootInfo, FIRMWARE_SEED, Framebuffer,
    KERNEL_VIRT_BASE, MemKind, MemRegion, PAGE_SIZE, PHYSMAP_ALIGN, PHYSMAP_BASE, PHYSMAP_END,
    allocator_owns, direct_map_runs, physmap_origin, read_only_span, split_run,
};
use ferrix_elf::{Class, EM_AARCH64, Elf, FixupKind, PF_W, PF_X, Segment};
use ferrix_paging::aarch64::{AArch64, MAIR_EL1};
use ferrix_paging::{MapFlags, Mapper, PhysAddr, PhysMem, VirtAddr};

use crate::entry::{self, Handoff};
use crate::kaslr;
use crate::log::say;
use crate::memory::{MAX_REGIONS, Memory, NO_REGION};
use crate::payload;
use crate::seed::{self, Seed};

/// Frames for page tables. Mapping eight gibibytes in 2 MiB blocks takes a
/// few dozen tables; this is ample.
const TABLE_POOL_BYTES: u64 = 4 * 1024 * 1024;

/// The boot info, the command line behind it, and the memory map after that,
/// laid out as `src/boot/common/uefi/` lays them out.
const BOOT_INFO_BYTES: u64 = 64 * 1024;

/// Offset of the memory map in the boot info area.
const REGIONS_OFFSET: u64 = PAGE_SIZE;

/// Offset of the command line in the boot info area.
const CMDLINE_OFFSET: u64 = size_of::<BootInfo>() as u64;

/// A range of physical memory the loader took.
#[derive(Clone, Copy, Debug)]
struct Taken {
    base: u64,
    len: u64,
}

/// What the caller hands to [`boot`] besides the memory map.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Carried<'a> {
    /// ABL's device tree.
    pub(crate) device_tree: &'a [u8],
    /// The kernel command line.
    pub(crate) cmdline: &'a str,
    /// The loader's own image, which the identity map covers.
    pub(crate) loader: (u64, u64),
    /// The framebuffer the display is showing, or [`Framebuffer::NONE`].
    /// `reclaimable` is filled in here, from the final memory map.
    pub(crate) framebuffer: Framebuffer,
    /// The random bytes in ABL's `/chosen`, removed from the kernel's copy.
    pub(crate) seed: Seed,
    /// Three words from TF-A's TRNG for the layout, or `None` for none.
    pub(crate) randomness: Option<[u64; 3]>,
}

/// Where the direct map begins, and the physical span it covers.
#[derive(Clone, Copy, Debug)]
struct DirectMap {
    /// The physical address at `base`.
    origin: u64,
    /// Bytes from there that are mapped.
    len: u64,
    /// Where it begins: [`PHYSMAP_BASE`] unless [`crate::kaslr`] moved it.
    base: u64,
}

impl DirectMap {
    /// The direct-map address of `phys`, which must not be below the origin.
    const fn address(self, phys: u64) -> u64 {
        self.base + (phys - self.origin)
    }
}

/// The page table pool: physical memory, used directly, because the MMU is
/// off.
#[derive(Debug)]
struct Tables {
    pool: Taken,
    used: u64,
}

// SAFETY: with the MMU off every physical address is a valid pointer. The
// frames come from a pool this type owns, are handed out at most once, are
// page aligned because the pool is, and were zeroed when it was taken.
unsafe impl PhysMem for Tables {
    fn read(&self, at: PhysAddr) -> u64 {
        // SAFETY: `at` is a descriptor in a table from the pool.
        unsafe { ptr::read_volatile(at.0 as *const u64) }
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        // SAFETY: as above, and nothing else refers to the pool.
        unsafe { ptr::write_volatile(at.0 as *mut u64, value) };
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let next = self.used.checked_add(PAGE_SIZE)?;
        if next > self.pool.len {
            return None;
        }
        let frame = PhysAddr(self.pool.base + self.used);
        self.used = next;
        Some(frame)
    }
}

/// Take `len` bytes of `kind`, drop anything a cache holds for them, and zero
/// them.
fn take(memory: &mut Memory, len: u64, kind: MemKind) -> Result<Taken, &'static str> {
    let len = len.next_multiple_of(PAGE_SIZE);
    let base = memory.allocate(len, kind)?;
    entry::invalidate_dcache(base, len);
    // SAFETY: the range was just allocated to the loader, is RAM, and nothing
    // else refers to it.
    unsafe { ptr::write_bytes(base as *mut u8, 0, len as usize) };
    Ok(Taken { base, len })
}

/// Copy `bytes` into a fresh allocation of `kind`.
fn take_copy(memory: &mut Memory, bytes: &[u8], kind: MemKind) -> Result<Taken, &'static str> {
    let taken = take(memory, bytes.len() as u64, kind)?;
    // SAFETY: the allocation is at least as long as `bytes` and is not part of
    // the loader's image, which `bytes` is.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), taken.base as *mut u8, bytes.len()) };
    Ok(taken)
}

/// Parse and vet the kernel, as `src/boot/common/uefi/` does.
fn parse_kernel(bytes: &[u8]) -> Result<Elf<'_>, &'static str> {
    if bytes.is_empty() {
        return Err("this loader was built without a kernel: set FERRIX_PIXEL7_KERNEL");
    }
    let elf = Elf::parse(bytes).map_err(|_| "the kernel is not a valid ELF image")?;
    if elf.class() != Class::Elf64 {
        return Err("the kernel was built for another word width");
    }
    elf.check_machine(EM_AARCH64)
        .map_err(|_| "the kernel was built for another architecture")?;
    // A PIE is moved where `kaslr` chooses, and its fixups applied for that
    // slide, which is none at all when nothing moved.
    elf.validate_segments()
        .map_err(|_| "the kernel has a malformed segment")?;
    Ok(elf)
}

/// Copy the kernel's segments into one allocation, laid out as linked. Its
/// fixups wait for [`apply_fixups`], once the slide is chosen.
fn place_kernel(memory: &mut Memory, elf: &Elf<'_>) -> Result<Taken, &'static str> {
    let (low, high) = elf
        .load_span(PAGE_SIZE)
        .ok_or("the kernel loads no segments")?;
    if low != KERNEL_VIRT_BASE {
        return Err("the kernel is not linked at the address the loader maps it to");
    }
    let image = take(memory, high - low, MemKind::Kernel)?;
    for segment in elf.loadable() {
        let data = segment
            .data(elf.image())
            .map_err(|_| "a kernel segment is out of bounds")?;
        let destination = image.base + (segment.vaddr - low);
        // SAFETY: inside the allocation, which was sized from the load span
        // this segment is part of; `.bss` is already zero.
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), destination as *mut u8, data.len()) };
    }
    Ok(image)
}

/// Apply a PIE's fixups for the image in `image`, linked at `low`, to run
/// `slide` bytes above it.
///
/// A `RELA` table keeps its addends in the table rather than in the words it
/// patches, so an unmoved PIE, `slide` zero, still needs this. Each is an aligned word,
/// written with one volatile store, as everything here is written with the
/// caches off.
fn apply_fixups(elf: &Elf<'_>, image: Taken, low: u64, slide: u64) -> Result<(), &'static str> {
    for fixup in elf.fixups() {
        let fixup = fixup.map_err(|_| "the kernel has a fixup this loader cannot apply")?;
        let FixupKind::Relative(_) = fixup.kind else {
            return Err("the kernel has a fixup this loader cannot apply");
        };
        let offset = fixup
            .at
            .checked_sub(low)
            .filter(|offset| offset.is_multiple_of(8) && offset + 8 <= image.len)
            .ok_or("a kernel fixup is outside the image, or unaligned")?;
        let at = (image.base + offset) as *mut u64;
        // SAFETY: an aligned word inside the image allocation, checked above,
        // which nothing else refers to yet.
        let stored = unsafe { ptr::read_volatile(at) };
        let value = fixup
            .apply(slide, stored)
            .ok_or("the kernel has a fixup this loader cannot apply")?;
        // SAFETY: as above.
        unsafe { ptr::write_volatile(at, value) };
    }
    Ok(())
}

/// The direct map a machine with this memory gets: from the lowest RAM,
/// rounded down to [`PHYSMAP_ALIGN`], to the highest, rounded up.
fn direct_map(regions: &[MemRegion]) -> Result<DirectMap, &'static str> {
    let ram = regions.iter().filter(|region| region.kind.is_ram());
    let low = ram.clone().map(|region| region.base).min();
    let high = ram.map(MemRegion::end).max();
    let (Some(low), Some(high)) = (low, high) else {
        return Err("the device tree describes no RAM");
    };
    let origin = physmap_origin(low);
    let len = (high.next_multiple_of(PHYSMAP_ALIGN) - origin).min(PHYSMAP_END - PHYSMAP_BASE);
    Ok(DirectMap {
        origin,
        len,
        base: PHYSMAP_BASE,
    })
}

/// Build the kernel's tree and the loader's identity tree.
fn build_tables(
    tables: &mut Tables,
    elf: &Elf<'_>,
    image: Taken,
    regions: &[MemRegion],
    direct: DirectMap,
    (loader, slide): ((u64, u64), u64),
) -> Result<(PhysAddr, PhysAddr), &'static str> {
    let kernel_root = tables
        .allocate_table()
        .ok_or("no frame for the kernel root")?;
    let identity_root = tables
        .allocate_table()
        .ok_or("no frame for the identity root")?;
    let kernel: Mapper<AArch64> = Mapper::new(kernel_root);
    let identity: Mapper<AArch64> = Mapper::new(identity_root);

    // What the switch fetches through at its own address: the loader, and
    // nothing else. Writable and executable, as `src/boot/common/uefi/`'s is, and dropped
    // with the rest of the tree.
    let transient = MapFlags {
        read: true,
        write: true,
        execute: true,
        user: false,
        global: false,
        device: false,
        uncached: false,
        write_combining: false,
    };
    let base = loader.0 & !(PAGE_SIZE - 1);
    let end = (loader.0 + loader.1).next_multiple_of(PAGE_SIZE);
    identity
        .map_range(
            tables,
            VirtAddr(base),
            PhysAddr(base),
            end - base,
            transient,
        )
        .map_err(|_| "could not build the identity map")?;

    // The kernel's text and read-only data read only in the direct map, as
    // `src/boot/common/uefi/` maps them; `read_only_span` says why.
    let sealed = read_only_span(elf.loadable().map(|segment| {
        let (base, length) = segment_pages(&segment);
        let phys = image.base + (base - KERNEL_VIRT_BASE);
        (phys, phys + length, segment.flags & PF_W != 0)
    }))?;
    for run in direct_map_runs(regions.iter().copied(), direct.origin, direct.len) {
        for (base, run, read_only) in split_run(run, sealed).into_iter().flatten() {
            let flags = if read_only {
                MapFlags::KERNEL_RODATA
            } else {
                MapFlags::KERNEL_DATA
            };
            kernel
                .map_range(
                    tables,
                    VirtAddr(direct.address(base)),
                    PhysAddr(base),
                    run,
                    flags,
                )
                .map_err(|_| "could not build the direct map")?;
        }
    }

    for segment in elf.loadable() {
        map_segment(&kernel, tables, &segment, image, slide)?;
    }
    Ok((kernel_root, identity_root))
}

/// Map one kernel segment with the permissions the linker gave it, `slide`
/// bytes above where it was linked.
fn map_segment(
    kernel: &Mapper<AArch64>,
    tables: &mut Tables,
    segment: &Segment,
    image: Taken,
    slide: u64,
) -> Result<(), &'static str> {
    let flags = MapFlags {
        read: true,
        write: segment.flags & PF_W != 0,
        execute: segment.flags & PF_X != 0,
        user: false,
        global: true,
        device: false,
        uncached: false,
        write_combining: false,
    };
    let (base, length) = segment_pages(segment);
    kernel
        .map_range(
            tables,
            VirtAddr(base + slide),
            PhysAddr(image.base + (base - KERNEL_VIRT_BASE)),
            length,
            flags,
        )
        .map_err(|_| "could not map a kernel segment")
}

/// The whole pages a segment occupies, as its first page's link address and a
/// length.
fn segment_pages(segment: &Segment) -> (u64, u64) {
    let base = segment.vaddr & !(PAGE_SIZE - 1);
    let length = (segment.vaddr - base + segment.memsz).next_multiple_of(PAGE_SIZE);
    (base, length)
}

/// `TCR_EL1` with 48-bit addressing and a 4 KiB granule in both halves, as
/// `src/boot/common/uefi/src/arch/aarch64.rs` builds it.
const fn tcr_el1(intermediate_physical_size: u64) -> u64 {
    const T0SZ: u64 = 64 - 48;
    const T1SZ: u64 = 64 - 48;
    const RGN_WRITE_BACK: u64 = 0b01;
    const SHAREABILITY_INNER: u64 = 0b11;
    const TG1_4K: u64 = 0b10;
    T0SZ | (RGN_WRITE_BACK << 8)
        | (RGN_WRITE_BACK << 10)
        | (SHAREABILITY_INNER << 12)
        | (T1SZ << 16)
        | (RGN_WRITE_BACK << 24)
        | (RGN_WRITE_BACK << 26)
        | (SHAREABILITY_INNER << 28)
        | (TG1_4K << 30)
        | (intermediate_physical_size << 32)
}

/// The kernel's allocations, for writing the boot info.
#[derive(Clone, Copy, Debug)]
struct Placed {
    image: Taken,
    stack: Taken,
    info: Taken,
    device_tree: (Taken, u64),
    initrd: Option<(Taken, u64)>,
    roots: (PhysAddr, PhysAddr),
    direct: DirectMap,
    choice: kaslr::Choice,
}

/// Write the boot info, the command line and the memory map into the info
/// area.
fn write_boot_info(
    placed: &Placed,
    regions: &[MemRegion],
    cmdline: &str,
    framebuffer: Framebuffer,
    seed: &Seed,
) {
    let direct = placed.direct;
    let info = placed.info;
    let cmdline_room = (REGIONS_OFFSET - CMDLINE_OFFSET) as usize;
    let cmdline = cmdline.get(..cmdline.len().min(cmdline_room)).unwrap_or("");
    // SAFETY: inside the info area, between the boot info and the map.
    unsafe {
        ptr::copy_nonoverlapping(
            cmdline.as_ptr(),
            (info.base + CMDLINE_OFFSET) as *mut u8,
            cmdline.len(),
        );
    }
    let capacity = ((info.len - REGIONS_OFFSET) / size_of::<MemRegion>() as u64) as usize;
    let count = regions.len().min(capacity);
    // SAFETY: inside the info area, `capacity` regions long from its offset.
    unsafe {
        ptr::copy_nonoverlapping(
            regions.as_ptr(),
            (info.base + REGIONS_OFFSET) as *mut MemRegion,
            count,
        );
    }
    let boot_info = BootInfo {
        magic: BOOTINFO_MAGIC,
        version: BOOTINFO_VERSION,
        arch: Arch::AArch64,
        regions: direct.address(info.base + REGIONS_OFFSET),
        regions_len: count as u64,
        physmap_base: direct.base,
        physmap_phys: direct.origin,
        physmap_len: direct.len,
        kernel_phys: placed.image.base,
        kernel_virt: placed.choice.kernel_virt,
        kernel_len: placed.image.len,
        root_table_phys: placed.roots.0.0,
        ttbr0_phys: placed.roots.1.0,
        loader_alias_phys: 0,
        loader_alias_len: 0,
        boot_stack_top: direct.address(placed.stack.base + placed.stack.len),
        boot_stack_size: placed.stack.len,
        framebuffer: Framebuffer {
            reclaimable: u32::from(
                framebuffer.is_present()
                    && allocator_owns(regions.iter().copied(), framebuffer.phys, framebuffer.size),
            ),
            ..framebuffer
        },
        initrd_phys: placed.initrd.map_or(0, |(taken, _)| taken.base),
        initrd_len: placed.initrd.map_or(0, |(_, len)| len),
        rsdp: 0,
        dtb: placed.device_tree.0.base,
        dtb_len: placed.device_tree.1,
        uefi_system_table: 0,
        cmdline: if cmdline.is_empty() {
            0
        } else {
            direct.address(info.base + CMDLINE_OFFSET)
        },
        cmdline_len: cmdline.len() as u64,
        firmware_time: 0,
        firmware_seed: seed.bytes,
        firmware_flags: if seed.is_any() { FIRMWARE_SEED } else { 0 },
        firmware_seed_len: seed.found as u64,
        kaslr: placed.choice.kaslr,
        // No region kept across a reset: the phone's bootloader is not UEFI,
        // and nothing reports what the previous kernel left (POSTMORTEM.md §3).
        pstore_phys: 0,
        pstore_len: 0,
        // The phone's bootloader is not UEFI and offers no SMBIOS table.
        smbios: 0,
        smbios_len: 0,
    };
    // SAFETY: the info area is the loader's, larger than one boot info.
    unsafe { ptr::write_volatile(info.base as *mut BootInfo, boot_info) };
}

/// Place Ferrix, describe the machine to it, and jump.
pub(crate) fn boot(memory: &mut Memory, carried: Carried<'_>) -> Result<(), &'static str> {
    let elf = parse_kernel(payload::kernel())?;
    let image = place_kernel(memory, &elf)?;
    let pool = take(memory, TABLE_POOL_BYTES, MemKind::PageTables)?;
    let stack = take(memory, BOOT_STACK_SIZE, MemKind::BootStack)?;
    let info = take(memory, BOOT_INFO_BYTES, MemKind::BootInfo)?;
    let device_tree = take_copy(memory, carried.device_tree, MemKind::DeviceTree)?;
    seed::remove(
        &carried.seed,
        device_tree.base,
        carried.device_tree.len() as u64,
    );
    let initrd = match payload::initrd() {
        [] => None,
        bytes => Some((
            take_copy(memory, bytes, MemKind::Initrd)?,
            bytes.len() as u64,
        )),
    };
    say!(
        "  kernel {:#x}+{:#x}, tables {:#x}, stack {:#x}, info {:#x}",
        image.base,
        image.len,
        pool.base,
        stack.base,
        info.base
    );

    // Nothing is allocated after this, so this is the map the kernel gets.
    let mut map = [NO_REGION; MAX_REGIONS];
    let count = memory.regions(&mut map);
    let regions = map.get(..count).unwrap_or(&[]);
    let direct = direct_map(regions)?;
    say!(
        "  {count} memory regions, direct map {:#x}+{:#x}",
        direct.origin,
        direct.len
    );

    // Where the image, the direct map and the arena go, now that the direct
    // map's size is known; the fixups follow the image's slide.
    let choice = kaslr::choose(
        &elf,
        (KERNEL_VIRT_BASE, image.len),
        direct.len,
        kaslr::declined(carried.cmdline),
        carried.randomness,
    )?;
    apply_fixups(&elf, image, KERNEL_VIRT_BASE, choice.slide())?;
    let direct = DirectMap {
        base: choice.physmap_base,
        ..direct
    };

    let mut tables = Tables { pool, used: 0 };
    let roots = build_tables(
        &mut tables,
        &elf,
        image,
        regions,
        direct,
        (carried.loader, choice.slide()),
    )?;
    say!("  {} page tables", tables.used / PAGE_SIZE);

    let placed = Placed {
        image,
        stack,
        info,
        device_tree: (device_tree, carried.device_tree.len() as u64),
        initrd,
        roots,
        direct,
        choice,
    };
    write_boot_info(
        &placed,
        regions,
        carried.cmdline,
        carried.framebuffer,
        &carried.seed,
    );

    let written = [image, pool, stack, info, device_tree];
    for taken in written
        .iter()
        .chain(initrd.as_ref().map(|(taken, _)| taken))
    {
        entry::invalidate_dcache(taken.base, taken.len);
    }
    let entry_point = elf.entry() + choice.slide();
    say!("entering Ferrix at {:#x}", entry_point);
    // SAFETY: at EL1 with the MMU off since the entry sequence; the identity
    // tree maps this loader's image, which this code is in; every range the
    // kernel is handed was invalidated just above, after its last write.
    unsafe {
        entry::enter_kernel(Handoff {
            mair: MAIR_EL1,
            tcr: tcr_el1(entry::physical_address_size()),
            identity_table: roots.1.0,
            root_table: roots.0.0,
            stack_top: direct.address(stack.base + stack.len),
            entry: entry_point,
            boot_info: direct.address(info.base),
        })
    }
}
