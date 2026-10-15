//! Placing the kernel and building the address space it will run in.

use core::ptr;

use ferrix_bootinfo::{
    IdentityPlan, IdentityTree, KERNEL_VIRT_BASE, LAYOUT, MemKind, MemRegion, PAGE_SIZE,
    PHYSMAP_ALIGN, PHYSMAP_BASE, PHYSMAP_END, Placement, direct_map_runs, physmap_origin,
    read_only_span, rsdp_region, split_run,
};
use ferrix_elf::{Elf, PF_W, PF_X, Segment};
use ferrix_paging::{MapFlags, Mapper, PhysAddr, PhysMem, VirtAddr};

use crate::arch::{self, PageEncoding};
use crate::services::{Allocation, BootError, MemoryMap, Result, Services};
use crate::uefi::tables::{MemoryDescriptor, MemoryType};

/// Frames set aside for page tables.
///
/// Two mebibytes is 512 tables, far more than the three mappings below need
/// even on a large machine. Taking it as one allocation rather than 512 keeps
/// the firmware memory map short: every separate `allocate_pages` adds a
/// descriptor to it, and the map has to be copied before the kernel starts.
const TABLE_POOL_BYTES: u64 = 2 * 1024 * 1024;

/// Physical memory as the loader sees it: identity mapped, because that is what
/// UEFI leaves in place, with a bump allocator for page table frames.
#[derive(Debug)]
pub(crate) struct LoaderMemory {
    pool: Allocation,
    used: u64,
}

impl LoaderMemory {
    /// Take a pool of frames to build page tables out of.
    pub(crate) fn new(services: &Services) -> Result<LoaderMemory> {
        let pool = services.allocate(
            "allocating the page table pool",
            TABLE_POOL_BYTES,
            MemoryType::FERRIX_PAGE_TABLES,
        )?;
        Ok(LoaderMemory { pool, used: 0 })
    }

    /// The pool, so the caller can clean it out of the caches before the MMU
    /// is turned off.
    pub(crate) const fn pool(&self) -> Allocation {
        self.pool
    }

    /// How many frames have been turned into page tables.
    pub(crate) const fn tables_used(&self) -> u64 {
        self.used / PAGE_SIZE
    }
}

// SAFETY: under boot services every physical address is identity mapped, so a
// physical address is a valid pointer. Frames come from a pool this type owns
// exclusively, are handed out at most once, are page aligned because the pool
// is, and were zeroed when the pool was allocated.
unsafe impl PhysMem for LoaderMemory {
    fn read(&self, at: PhysAddr) -> u64 {
        // SAFETY: `at` is a descriptor inside a table from our own pool, which
        // the identity map makes a valid, aligned pointer.
        unsafe { ptr::read_volatile(at.0 as *const u64) }
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        // SAFETY: as above, and nothing else holds a reference to the pool.
        unsafe { ptr::write_volatile(at.0 as *mut u64, value) };
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let next = self.used.checked_add(PAGE_SIZE)?;
        if next > self.pool.len {
            return None;
        }
        let frame = PhysAddr(self.pool.address + self.used);
        self.used = next;
        Some(frame)
    }
}

/// Read the kernel off the boot volume into memory the loader owns.
///
/// Returns the allocation and the file's real length, which is smaller than
/// the allocation unless the file happens to be a whole number of pages.
pub(crate) fn read_kernel_file(services: &Services, path: &str) -> Result<(Allocation, u64)> {
    services.read_file(path, MemoryType::LOADER_DATA)
}

/// Parse and vet a kernel image before anything is mapped.
pub(crate) fn parse_kernel(bytes: &[u8]) -> Result<Elf<'_>> {
    let elf =
        Elf::parse(bytes).map_err(|_| BootError::plain("the kernel is not a valid ELF image"))?;
    if elf.class() != arch::ELF_CLASS {
        return Err(BootError::plain(
            "the kernel was built for another word width",
        ));
    }
    elf.check_machine(arch::ELF_MACHINE)
        .map_err(|_| BootError::plain("the kernel was built for another architecture"))?;
    // A position-independent kernel is accepted: `place_kernel` applies its
    // fixups, wherever it goes, including to its own link address.
    elf.validate_segments()
        .map_err(|_| BootError::plain("the kernel has a malformed segment"))?;
    Ok(elf)
}

/// Where the kernel image ended up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KernelImage {
    /// Physical memory holding the image.
    pub(crate) memory: Allocation,
    /// Lowest virtual address the image occupies: where it was linked, or
    /// where KASLR moved it.
    pub(crate) virt_base: u64,
    /// Lowest virtual address the image was linked at.
    pub(crate) link: u64,
    /// Virtual address of the entry point, moved with the image.
    pub(crate) entry: u64,
}

impl KernelImage {
    /// Nothing placed yet.
    pub(crate) const EMPTY: KernelImage = KernelImage {
        memory: Allocation { address: 0, len: 0 },
        virt_base: 0,
        link: 0,
        entry: 0,
    };
}

/// The kernel's link address and the bytes its segments span, page rounded.
pub(crate) fn kernel_span(elf: &Elf<'_>) -> Result<(u64, u64)> {
    let (low, high) = elf
        .load_span(PAGE_SIZE)
        .ok_or_else(|| BootError::plain("the kernel loads no segments"))?;
    if low != KERNEL_VIRT_BASE {
        return Err(BootError::plain(
            "the kernel is not linked at the address the loader maps it to",
        ));
    }
    Ok((low, high - low))
}

/// Copy the kernel where it will run, and patch it to run at `virt`.
///
/// The image goes wherever firmware offers physical memory and is mapped at
/// `virt` — its link address, or where [`crate::kaslr`] moved it — so nothing
/// here depends on a physical layout. Every fixup is applied, even when the
/// image did not move: a PIE's `RELA` table carries its addends in the table,
/// not in the words it patches.
pub(crate) fn place_kernel(services: &Services, elf: &Elf<'_>, virt: u64) -> Result<KernelImage> {
    let (low, len) = kernel_span(elf)?;

    let memory = services.allocate(
        "allocating the kernel image",
        len,
        MemoryType::FERRIX_KERNEL,
    )?;

    for segment in elf.loadable() {
        copy_segment(elf, &segment, memory.address, low)?;
    }
    let bias = virt.wrapping_sub(low);
    apply_fixups(elf, memory, low, bias)?;

    Ok(KernelImage {
        memory,
        virt_base: virt,
        link: low,
        entry: elf.entry().wrapping_add(bias),
    })
}

/// Patch every place `elf` says has to change for the image in `memory`,
/// linked at `low`, to run moved by `bias`.
fn apply_fixups(elf: &Elf<'_>, memory: Allocation, low: u64, bias: u64) -> Result<()> {
    let refused = || BootError::plain("the kernel's fixups are of a kind the loader cannot apply");
    let outside = || BootError::plain("a kernel fixup is outside the image");
    for fixup in elf.fixups() {
        let fixup = fixup.map_err(|_| refused())?;
        let width = fixup.width(elf.class()) as u64;
        let offset = fixup.at.checked_sub(low).ok_or_else(outside)?;
        if offset.checked_add(width).is_none_or(|end| end > memory.len) {
            return Err(outside());
        }
        let at = memory.address + offset;
        let stored = if width == 8 {
            // SAFETY: `at` is inside the image allocation, checked above; the
            // image is the loader's own and nothing else refers to it.
            unsafe { ptr::read_unaligned(at as *const u64) }
        } else {
            // SAFETY: as above, for a 4-byte word.
            u64::from(unsafe { ptr::read_unaligned(at as *const u32) })
        };
        let value = fixup.apply(bias, stored).ok_or_else(refused)?;
        if width == 8 {
            // SAFETY: as above.
            unsafe { ptr::write_unaligned(at as *mut u64, value) };
        } else {
            // SAFETY: as above, for a 4-byte word; `apply` gives a 32-bit
            // value for every 4-byte fixup, so nothing is cut off.
            unsafe { ptr::write_unaligned(at as *mut u32, value as u32) };
        }
    }
    Ok(())
}

/// Copy one segment into the image and zero the `.bss` tail behind it.
fn copy_segment(elf: &Elf<'_>, segment: &Segment, base: u64, virt_base: u64) -> Result<()> {
    let data = segment
        .data(elf.image())
        .map_err(|_| BootError::plain("a kernel segment is out of bounds"))?;

    let offset = segment.vaddr - virt_base;
    let destination = base + offset;

    // SAFETY: `destination` is inside the allocation, because `offset` is within
    // the load span the allocation was sized from; the file buffer and the image
    // are separate allocations, so they cannot overlap.
    unsafe { ptr::copy_nonoverlapping(data.as_ptr(), destination as *mut u8, data.len()) };

    // Everything past `p_filesz` is `.bss` and must read as zero. The allocation
    // was already zeroed, so this is belt and braces — but a loader that relies
    // on someone else having zeroed memory is a loader with an intermittent bug.
    let tail = segment.memsz - segment.filesz;
    if tail != 0 {
        // SAFETY: the tail is inside the same allocation, for the same reason.
        unsafe { ptr::write_bytes((destination + segment.filesz) as *mut u8, 0, tail as usize) };
    }
    Ok(())
}

/// The part of physical memory the direct map spans.
///
/// From the lowest RAM address, rounded down to [`PHYSMAP_ALIGN`], to the
/// highest, rounded up — and no further than the direct map's region of the
/// address space holds, which only binds on a 32-bit machine. Starting at the
/// lowest RAM rather than at zero is what lets a 32-bit kernel afford a direct
/// map at all: QEMU's Arm `virt` machines have nothing but flash and device
/// registers below 1 GiB, and mapping that as cacheable memory was never right
/// on AArch64 either.
///
/// For the same reason, only what the memory map describes as memory is mapped
/// inside the span, and a hole between RAM banks is not; see
/// [`direct_map_runs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirectMap {
    /// The physical address at `base`.
    pub(crate) origin: u64,
    /// Bytes from there that are mapped.
    pub(crate) len: u64,
    /// Where it begins: [`PHYSMAP_BASE`] until [`crate::kaslr`] moves it.
    pub(crate) base: u64,
}

impl DirectMap {
    /// The direct map a machine with this memory map gets.
    pub(crate) fn of(map: &MemoryMap) -> Result<DirectMap> {
        let mut low = u64::MAX;
        let mut high = 0u64;
        for descriptor in map.entries() {
            if !describe(&descriptor).kind.is_ram() {
                continue;
            }
            low = low.min(descriptor.physical_start);
            high = high.max(descriptor.physical_start + descriptor.number_of_pages * PAGE_SIZE);
        }
        if high == 0 {
            return Err(BootError::plain("firmware's memory map describes no RAM"));
        }

        let origin = physmap_origin(low);
        let end = high.next_multiple_of(PHYSMAP_ALIGN);
        Ok(DirectMap {
            origin,
            len: (end - origin).min(PHYSMAP_END - PHYSMAP_BASE),
            base: PHYSMAP_BASE,
        })
    }

    /// The same span of RAM, mapped from `base`.
    pub(crate) const fn at(self, base: u64) -> DirectMap {
        DirectMap { base, ..self }
    }

    /// Check that `map`, fetched after the loader's allocations, gives the same
    /// direct map as the one measured before them.
    ///
    /// It must: allocating turns free RAM into the loader's own types, which
    /// are RAM too, so the span cannot have moved. A firmware for which it did
    /// would have placed the kernel against a ceiling that no longer holds.
    pub(crate) fn confirm(self, map: &MemoryMap) -> Result<()> {
        if DirectMap::of(map)?.at(self.base) == self {
            Ok(())
        } else {
            Err(BootError::plain(
                "the direct map changed while the loader was allocating",
            ))
        }
    }

    /// Where physical address `phys` appears in the direct map.
    pub(crate) const fn address(self, phys: u64) -> u64 {
        self.base + (phys - self.origin)
    }

    /// One past the last physical address the direct map covers.
    pub(crate) const fn end(self) -> u64 {
        self.origin + self.len
    }

    /// True if every byte of `allocation` is inside the direct map, which is
    /// the only way the kernel can reach it.
    pub(crate) const fn covers(self, allocation: Allocation) -> bool {
        allocation.address >= self.origin && allocation.address + allocation.len <= self.end()
    }
}

/// Where the switch runs from, and so what has to be identity mapped for it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Switch {
    /// The mapping `build_address_space` makes.
    pub(crate) identity: IdentityPlan,
    /// A page holding a copy of the switch, when the loader's own image could
    /// not be mapped at its own address.
    pub(crate) trampoline: Option<Allocation>,
}

impl Switch {
    /// Clean the trampoline, if there is one, out of the data cache: it was
    /// written as data and is about to be run with the caches off.
    pub(crate) fn clean(self) {
        if let Some(page) = self.trampoline {
            arch::clean_dcache(page.address, page.len);
        }
    }
}

/// Decide where the switch runs from, and copy it there if that is not here.
///
/// Where the loader may map itself is a property of the machine's memory, and
/// is decided in `src/lib/proto/bootinfo` so that the answer for a board nobody here can
/// boot is still something the host tests pin down. When the answer is a
/// trampoline, the page comes from RAM below the split and below the direct
/// map's ceiling, and holds the switch's instructions and nothing else.
pub(crate) fn place_switch(
    services: &Services,
    direct: DirectMap,
    loader: (u64, u64),
    (kernel_base, kernel_len): (u64, u64),
) -> Result<Switch> {
    let placed = Placement {
        physmap_base: direct.base,
        kernel_base,
    };
    let plan = LAYOUT
        .plan_identity_map_in(
            placed,
            direct.origin,
            direct.len,
            loader.0,
            loader.1,
            kernel_len,
        )
        .map_err(BootError::plain)?;
    if plan.tree != IdentityTree::Trampoline {
        return Ok(Switch {
            identity: plan,
            trampoline: None,
        });
    }

    let code = arch::switch_code()
        .ok_or_else(|| BootError::plain("this architecture has no switch to copy"))?;
    let page = services.allocate_under(
        "allocating the switch trampoline",
        PAGE_SIZE,
        MemoryType::LOADER_DATA,
        plan.base + plan.len,
    )?;
    if page.address < plan.base || code.len() as u64 > page.len {
        return Err(BootError::plain(
            "the switch trampoline is not a page below the split",
        ));
    }
    // SAFETY: the page was just allocated to us and is identity mapped under
    // boot services; `code` is the loader's own text, which does not overlap
    // it, and its length was checked against the page's.
    unsafe {
        ptr::copy_nonoverlapping(code.as_ptr(), page.address as *mut u8, code.len());
    }
    Ok(Switch {
        // Mapped by `map_identity` like any identity plan: normal memory,
        // readable, writable and executable, as the loader's own image would
        // have been, and dropped with the rest of the separate tree.
        identity: IdentityPlan {
            tree: IdentityTree::Separate,
            base: page.address,
            len: page.len,
        },
        trampoline: Some(page),
    })
}

/// The page tables the kernel will start on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AddressSpace {
    /// Root of the kernel half: x86-64's PML4, `AArch64`'s `TTBR1_EL1` table,
    /// or the ARMv7-A root `TTBR1` translates the last two entries of.
    pub(crate) kernel_root: PhysAddr,
    /// Root of the identity map, which on x86-64 is the same table.
    pub(crate) identity_root: PhysAddr,
    /// The identity mapping that had to go in the kernel's own tree, as an
    /// address and a length, on a machine whose RAM is above the split. The
    /// kernel is told, because it is the one that unmaps it.
    pub(crate) loader_alias: Option<(u64, u64)>,
}

/// Build the address space described in `docs/ARCHITECTURE.md` §4.
///
/// Three mappings, and deliberately no more: an identity map so the
/// instructions after the switch still fetch, a direct map of physical
/// memory, and the kernel image where [`crate::kaslr`] put it. The boot
/// stack and the boot info need no mapping of their own — they are in RAM, so
/// the direct map already covers them. The direct map is writable except
/// over the image's text and read-only data, which it aliases read only.
///
/// `map` is the memory map `direct` was measured from, and says which parts of
/// that span are memory at all. `plan` is where the switch is identity mapped,
/// from [`place_switch`]. `rsdp` is firmware's root table pointer, or zero,
/// whose pages are mapped whatever `map` says; see [`rsdp_region`].
pub(crate) fn build_address_space(
    memory: &mut LoaderMemory,
    elf: &Elf<'_>,
    image: &KernelImage,
    (direct, map): (DirectMap, &MemoryMap),
    plan: IdentityPlan,
    rsdp: u64,
) -> Result<AddressSpace> {
    let kernel_root = memory
        .allocate_table()
        .ok_or_else(|| BootError::plain("no frame for the kernel root table"))?;
    let identity_root = if arch::SEPARATE_IDENTITY_TABLE {
        memory
            .allocate_table()
            .ok_or_else(|| BootError::plain("no frame for the identity root table"))?
    } else {
        kernel_root
    };

    let kernel: Mapper<PageEncoding> = Mapper::new(kernel_root);
    let identity: Mapper<PageEncoding> = Mapper::new(identity_root);

    map_identity(&kernel, &identity, memory, plan)?;
    // Run by run, so that a device window or a hole between RAM banks is not
    // mapped as memory; `direct_map_runs` says why that matters.
    // And the RSDP's pages, which the kernel reads first and which firmware
    // on a PC may leave out of the map.
    let regions = map
        .entries()
        .map(|descriptor| describe(&descriptor))
        .chain(rsdp_region(rsdp));
    // And the kernel's own text and read-only data read only in it, as they
    // are in the image mapping; `read_only_span` says why.
    let sealed = read_only_span(elf.loadable().map(|segment| {
        let (base, length) = segment_pages(&segment);
        let phys = image.memory.address + (base - image.link);
        (phys, phys + length, segment.flags & PF_W != 0)
    }))
    .map_err(BootError::plain)?;
    for run in direct_map_runs(regions, direct.origin, direct.len) {
        for (base, len, read_only) in split_run(run, sealed).into_iter().flatten() {
            // Never executable: nothing is ever run through the direct map,
            // and it covers every byte of RAM including the kernel's own text.
            let flags = if read_only {
                MapFlags::KERNEL_RODATA
            } else {
                MapFlags::KERNEL_DATA
            };
            kernel
                .map_range(
                    memory,
                    VirtAddr(direct.address(base)),
                    PhysAddr(base),
                    len,
                    flags,
                )
                .map_err(|_| BootError::plain("could not build the direct map"))?;
        }
    }

    for segment in elf.loadable() {
        map_segment(&kernel, memory, &segment, image)?;
    }

    Ok(AddressSpace {
        kernel_root,
        identity_root,
        loader_alias: matches!(plan.tree, IdentityTree::Kernel).then_some((plan.base, plan.len)),
    })
}

/// Map what the switch fetches through at its own address, temporarily and
/// executably.
///
/// Usually that is all of RAM, in a tree of the loader's own: the loader's
/// code, its stack and its tables are all in RAM, and nothing else has to be
/// reachable at its physical address for the few instructions between the
/// switch and the jump.
///
/// On a machine whose RAM is above the split — the STM32MP157, whose DDR
/// starts at 3 GiB — those addresses are the kernel's half, which the tree of
/// the loader's own does not translate at all. There the mapping is the
/// loader's own image, in the kernel's tree, and the kernel unmaps it when it
/// drops the rest of the identity map. [`ferrix_bootinfo::Layout`] is where
/// the two cases are told apart.
fn map_identity(
    kernel: &Mapper<PageEncoding>,
    identity: &Mapper<PageEncoding>,
    memory: &mut LoaderMemory,
    plan: IdentityPlan,
) -> Result<()> {
    // This mapping has to be executable, which is otherwise never true in this
    // tree: the instruction after the one that installs these tables is fetched
    // through it, and on the Arm architectures so is the whole sequence that
    // turns the MMU back on. It is transient — the kernel drops it once it is
    // running on its own stack, and `docs/ARCHITECTURE.md` §4 says so.
    //
    // Writable as well as executable in both cases, and deliberately: the
    // kernel's W^X sweep requires the identity map to be a violation it can
    // see before it is dropped, which is what makes the sweep's later silence
    // mean something.
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
    let mapper = match plan.tree {
        IdentityTree::Separate => identity,
        IdentityTree::Kernel => kernel,
        // `place_switch` turns a trampoline plan into a separate-tree mapping
        // of the page it chose, so none reaches here.
        IdentityTree::Trampoline => {
            return Err(BootError::plain(
                "a trampoline plan was never given its page",
            ));
        }
    };
    mapper
        .map_range(
            memory,
            VirtAddr(plan.base),
            PhysAddr(plan.base),
            plan.len,
            transient,
        )
        .map_err(|_| BootError::plain("could not build the identity map"))
}

/// Map one kernel segment with the permissions the linker gave it.
fn map_segment(
    kernel: &Mapper<PageEncoding>,
    memory: &mut LoaderMemory,
    segment: &Segment,
    image: &KernelImage,
) -> Result<()> {
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
    let offset = base - image.link;

    kernel
        .map_range(
            memory,
            VirtAddr(image.virt_base + offset),
            PhysAddr(image.memory.address + offset),
            length,
            flags,
        )
        .map_err(|_| BootError::plain("could not map a kernel segment"))
}

/// The whole pages a segment occupies, as its first page's link address and a
/// length.
///
/// The linker page-aligns segments, so rounding the base down and the length
/// up cannot make two segments overlap — and if a future linker script broke
/// that, the mapper would refuse rather than silently re-permission a page.
fn segment_pages(segment: &Segment) -> (u64, u64) {
    let base = segment.vaddr & !(PAGE_SIZE - 1);
    let length = (segment.vaddr - base + segment.memsz).next_multiple_of(PAGE_SIZE);
    (base, length)
}

/// Convert one firmware memory descriptor into a Ferrix memory region.
pub(crate) fn describe(descriptor: &MemoryDescriptor) -> MemRegion {
    // The numeric values are UEFI's, including the ones the loader assigned
    // itself out of the range the specification reserves for an OS loader.
    let kind = match descriptor.memory_type {
        7 => MemKind::Usable,
        // Boot services code and data become free the moment we exit them, and
        // on most firmware that is a large fraction of low memory.
        3 | 4 => MemKind::Usable,
        1 | 2 => MemKind::Loader,
        9 => MemKind::AcpiReclaim,
        10 => MemKind::AcpiNvs,
        11 | 12 => MemKind::Mmio,
        8 => MemKind::Defective,
        0x8000_0000 => MemKind::Kernel,
        0x8000_0001 => MemKind::PageTables,
        0x8000_0002 => MemKind::BootStack,
        0x8000_0003 => MemKind::BootInfo,
        0x8000_0004 => MemKind::Initrd,
        0x8000_0005 => MemKind::DeviceTree,
        0x8000_0006 => MemKind::Pstore,
        _ => MemKind::Reserved,
    };

    MemRegion {
        base: descriptor.physical_start,
        len: descriptor.number_of_pages * PAGE_SIZE,
        kind,
        reserved: 0,
    }
}
