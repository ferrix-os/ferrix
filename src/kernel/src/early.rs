//! Physical memory and page tables before there is a frame allocator.
//!
//! The kernel is entered with three mappings and no way to make a fourth: the
//! loader's page table pool belongs to the loader, and the buddy allocator does
//! not exist until stage 2. That is a problem immediately, because on `AArch64`
//! the console is an `MMIO` register that has to be mapped as device memory
//! before it can be written.
//!
//! So the kernel carries its own frames, in `.bss`, and reaches physical memory
//! through the direct map the loader left. That is enough to install a mapping,
//! which is all early boot needs.

use core::cell::UnsafeCell;

use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_paging::{MapFlags, Mapper, PhysAddr, PhysMem, VirtAddr};

use crate::arch::{self, PageEncoding};

/// Page table frames carried in `.bss`.
///
/// Sixteen is enough for a handful of device windows at any depth of the walk:
/// each new mapping needs at most three tables, and early boot makes two or
/// three mappings. Running out is reported, never ignored.
const EARLY_TABLES: usize = 16;

/// One page table, aligned so it can be one.
#[repr(C, align(4096))]
struct Table([u64; 512]);

/// The pool itself.
struct TablePool(UnsafeCell<[Table; EARLY_TABLES]>);

// SAFETY: (SHARED) early boot is single-threaded — no other CPU has been started, and
// interrupts are masked — so there is never a second accessor. `EarlyMemory`
// hands out each frame at most once, and nothing else in the kernel refers to
// this static.
unsafe impl Sync for TablePool {}

static POOL: TablePool = TablePool(UnsafeCell::new(
    // `[Table([0; 512]); 16]` would need `Copy`, which a page table should not
    // have: copying one silently duplicates a mapping.
    [const { Table([0; 512]) }; EARLY_TABLES],
));

/// Why an early mapping could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EarlyError {
    /// The mapper refused the request. A pool in `.bss` that has run dry
    /// arrives here too, as `MapError::OutOfMemory`.
    MapFailed(ferrix_paging::MapError),
    /// The machine's description names no console this kernel can drive.
    #[allow(
        dead_code,
        reason = "only an architecture that looks its console up can fail to find one"
    )]
    NoConsole,
    /// A device window was asked for over the kernel's own image, at this
    /// physical address: see [`crate::mm::overlaps_image`].
    KernelImage(u64),
}

/// Physical memory as the kernel sees it during early boot.
#[derive(Debug)]
pub struct EarlyMemory {
    /// Base of the loader's direct map of physical memory.
    physmap: u64,
    /// The physical address that appears at `physmap`.
    physmap_phys: u64,
    /// Where the kernel image is mapped, and where it physically is — the two
    /// together are what turns a `.bss` address into a physical frame number.
    kernel_virt: u64,
    kernel_phys: u64,
    /// Bytes of the kernel image, from `kernel_phys`: what no device window
    /// may map.
    kernel_len: u64,
    /// Physical address of the root page table the loader installed.
    root: PhysAddr,
    /// Frames handed out of [`POOL`] so far.
    used: usize,
}

impl EarlyMemory {
    /// Build from the loader's hand-off.
    pub fn new(view: &BootView<'_>) -> EarlyMemory {
        let info = view.raw();
        EarlyMemory {
            physmap: info.physmap_base,
            physmap_phys: info.physmap_phys,
            kernel_virt: info.kernel_virt,
            kernel_phys: info.kernel_phys,
            kernel_len: info.kernel_len,
            root: PhysAddr(info.root_table_phys),
            used: 0,
        }
    }

    /// The physical address a kernel-image virtual address corresponds to.
    ///
    /// Only valid for addresses inside the kernel image, which is the only
    /// region whose virtual-to-physical relationship is a constant offset.
    const fn kernel_phys_of(&self, virt: u64) -> u64 {
        virt - self.kernel_virt + self.kernel_phys
    }

    /// Where physical address `phys` is readable: in the direct map, which
    /// begins at the lowest RAM address rather than at zero.
    const fn direct(&self, phys: u64) -> u64 {
        self.physmap + (phys - self.physmap_phys)
    }

    /// Read one byte through the direct map.
    ///
    /// Used by the boot self-check to prove the direct map really does alias
    /// the same memory the kernel image is mapped from.
    pub fn read_physical_byte(&self, phys: u64) -> u8 {
        // SAFETY: (FRAME) the loader mapped every byte of RAM into the direct map, and
        // the caller is reading an address that came from the memory map.
        unsafe { core::ptr::read_volatile(self.direct(phys) as *const u8) }
    }

    /// Resolve a virtual address the way the hardware would.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        let mapper: Mapper<PageEncoding> = Mapper::new(self.root);
        mapper.translate(self, VirtAddr(virt)).map(|phys| phys.0)
    }

    /// Map `len` bytes of device registers at `virt`.
    ///
    /// Device memory rather than normal memory: an `MMIO` register reached
    /// through a cacheable mapping can be merged, reordered or simply not
    /// written, and the symptom is a console that prints nothing.
    ///
    /// # Errors
    ///
    /// [`EarlyError::MapFailed`] with
    /// [`ferrix_paging::MapError::RangeOverflow`] for a range whose end,
    /// rounded out to a page, is past the top of the address space, before
    /// anything else is asked of it: a wrapped `phys + len` would otherwise be
    /// judged against the image by its clamped end, and the rounding would
    /// stop the kernel on the overflow check.
    /// [`EarlyError::KernelImage`] for a range that touches the kernel's own
    /// image, which no device window may map ([`crate::mm::overlaps_image`]);
    /// [`EarlyError::MapFailed`] for what the mapper refuses.
    pub fn map_device(&mut self, virt: u64, phys: u64, len: u64) -> Result<(), EarlyError> {
        self.map_window(virt, phys, len, MapFlags::KERNEL_DEVICE)
    }

    /// Map `len` bytes of a framebuffer at `virt`, with the attributes
    /// [`arch::FRAMEBUFFER_FLAGS`] gives: write-combining where the
    /// architecture has it, so the boot console's stores may gather into
    /// bursts rather than go out one at a time.
    ///
    /// # Errors
    ///
    /// As [`EarlyMemory::map_device`].
    pub fn map_framebuffer(
        &mut self,
        virt: u64,
        phys: u64,
        len: u64,
    ) -> Result<(), EarlyError> {
        self.map_window(virt, phys, len, arch::FRAMEBUFFER_FLAGS)
    }

    /// [`EarlyMemory::map_device`] with the attributes `flags` gives.
    fn map_window(
        &mut self,
        virt: u64,
        phys: u64,
        len: u64,
        flags: MapFlags,
    ) -> Result<(), EarlyError> {
        let span = len
            .checked_next_multiple_of(PAGE_SIZE)
            .filter(|&span| phys.checked_add(span).is_some())
            .ok_or(EarlyError::MapFailed(
                ferrix_paging::MapError::RangeOverflow,
            ))?;
        if crate::mm::image_span_overlaps(self.kernel_phys, self.kernel_len, phys, len) {
            return Err(EarlyError::KernelImage(phys));
        }
        let mapper: Mapper<PageEncoding> = Mapper::new(self.root);
        mapper
            .map_range(self, VirtAddr(virt), PhysAddr(phys), span, flags)
            .map_err(EarlyError::MapFailed)?;

        // The page table walker is a separate observer of memory. On AArch64 it
        // will not see a descriptor that is still sitting in a store buffer, so
        // the barrier inside `flush_tlb` is what makes the mapping real.
        arch::flush_tlb();
        Ok(())
    }
}

// SAFETY: (TRANSLATE) `read` and `write` go through the loader's direct map, which covers
// every byte of physical RAM and is the only mapping of it the kernel holds, so
// no reference can alias them. `allocate_table` hands out each frame of `POOL`
// at most once; the frames are page aligned because `Table` is, and zeroed
// because `.bss` is.
unsafe impl PhysMem for EarlyMemory {
    fn read(&self, at: PhysAddr) -> u64 {
        // SAFETY: (TRANSLATE) as above; `at` is a descriptor address inside a page table.
        unsafe { core::ptr::read_volatile(self.direct(at.0) as *const u64) }
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        // SAFETY: (TRANSLATE) as above.
        unsafe { core::ptr::write_volatile(self.direct(at.0) as *mut u64, value) };
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        if self.used >= EARLY_TABLES {
            return None;
        }
        let index = self.used;
        self.used += 1;

        // Pointer arithmetic rather than indexing: `xs[i]` is a panic the type
        // system does not catch, and this runs before there is anything to
        // report a panic with.
        let base: *mut Table = POOL.0.get().cast::<Table>();
        // SAFETY: (SHARED) single-threaded early boot, as documented on the `Sync` impl
        // for `TablePool`, and `index` is below `EARLY_TABLES`, so the result
        // is inside the array.
        let table = unsafe { base.add(index) };
        Some(PhysAddr(self.kernel_phys_of(table as u64)))
    }
}
