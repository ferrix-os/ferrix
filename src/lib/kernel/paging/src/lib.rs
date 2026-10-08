//! Page table construction for x86-64, AArch64 and ARMv7-A, and for the
//! IOMMUs that translate their devices' DMA: VT-d's second-level tables and an
//! `SMMUv3`'s stage 2, both three-level walks over 39 bits of I/O address.
//!
//! All three use tables of 512 eight-byte descriptors over a 4 KiB granule,
//! indexed by the same bits of the virtual address — ARMv7-A by way of the
//! Large Physical Address Extension, whose long descriptors are AArch64's with
//! a narrower address. Two things differ: the *encoding* of a descriptor, and
//! the *geometry* of the walk, which is four levels over 48 bits on the 64-bit
//! pair and three over 32 on ARMv7-A, starting at what the others call level
//! one. So the walk is written once here, generic over an [`Encoding`] that
//! supplies both, and each architecture is roughly forty lines of bit layout.
//!
//! This lives in `src/lib/` rather than in the kernel for the reason
//! `docs/ARCHITECTURE.md` gives: it is pure arithmetic over bytes, so
//! `cargo test`, Miri and a fuzzer can all reach it, and a mistake here is the
//! kind that writes to the wrong physical page and shows up somewhere else
//! entirely.
//!
//! # Physical and virtual addresses are different types
//!
//! [`PhysAddr`] and [`VirtAddr`] are newtypes with no arithmetic between them.
//! Confusing the two is this project's characteristic bug, and it is the one
//! the compiler can be made to catch for free.
//!
//! ```
//! # use ferrix_paging::{Mapper, MapFlags, PhysAddr, VirtAddr, x86_64::X86_64};
//! # fn example(memory: &mut impl ferrix_paging::PhysMem) -> Result<(), ferrix_paging::MapError> {
//! let root = memory.allocate_table().ok_or(ferrix_paging::MapError::OutOfMemory)?;
//! let mut mapper: Mapper<X86_64> = Mapper::new(root);
//! mapper.map_range(
//!     memory,
//!     VirtAddr(0xFFFF_FFFF_8000_0000),
//!     PhysAddr(0x20_0000),
//!     0x20_0000,
//!     MapFlags::KERNEL_CODE,
//! )?;
//! # Ok(())
//! # }
//! ```

#![no_std]

pub mod aarch64;
pub mod armv7a;
pub mod asid;
pub mod coherence;
pub mod stage2;
pub mod vtd;
pub mod x86_64;

use core::fmt;
use core::marker::PhantomData;

/// Bytes in the smallest mapping any architecture is configured for.
pub const PAGE_SIZE: u64 = 4096;

/// Descriptors in one table. 4 KiB / 8 bytes.
pub const ENTRIES: usize = 512;

/// Bits of virtual address a four-level 4 KiB-granule table walk covers, and
/// so the default [`Encoding::VIRT_BITS`].
pub const VIRT_BITS: u32 = 48;

/// A physical address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PhysAddr(pub u64);

/// A virtual address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VirtAddr(pub u64);

impl PhysAddr {
    /// True if the address is a multiple of `alignment`.
    #[must_use]
    pub const fn is_aligned_to(self, alignment: u64) -> bool {
        self.0 & (alignment - 1) == 0
    }

    /// This address advanced by `bytes`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, bytes: u64) -> Option<PhysAddr> {
        match self.0.checked_add(bytes) {
            Some(sum) => Some(PhysAddr(sum)),
            None => None,
        }
    }
}

impl VirtAddr {
    /// True if the address is a multiple of `alignment`.
    #[must_use]
    pub const fn is_aligned_to(self, alignment: u64) -> bool {
        self.0 & (alignment - 1) == 0
    }

    /// This address advanced by `bytes`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, bytes: u64) -> Option<VirtAddr> {
        match self.0.checked_add(bytes) {
            Some(sum) => Some(VirtAddr(sum)),
            None => None,
        }
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#018x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#018x})", self.0)
    }
}

/// Which level of the walk a descriptor belongs to.
///
/// Level 0 is the root of a four-level walk — the x86-64 PML4 and the AArch64
/// level-0 table — and level 3 holds the 4 KiB leaves. A shorter walk starts
/// further down rather than renumbering: ARMv7-A's root is level 1, and that
/// is [`Encoding::ROOT_LEVEL`]. Naming a level by what one of its descriptors
/// *covers* rather than by its distance from the root is what keeps a 2 MiB
/// block at level 2 on every architecture.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Level(u8);

impl Level {
    /// The root of a four-level walk.
    pub const ROOT: Level = Level(0);
    /// Where a 1 GiB block may be mapped.
    pub const GIGABYTE: Level = Level(1);
    /// Where a 2 MiB block may be mapped.
    pub const MEGABYTE: Level = Level(2);
    /// Where 4 KiB pages are mapped.
    pub const PAGE: Level = Level(3);

    /// Construct a level, `None` outside 0..=3.
    #[must_use]
    pub const fn new(level: u8) -> Option<Level> {
        if level <= 3 { Some(Level(level)) } else { None }
    }

    /// The level as a number, 0 at the root.
    #[must_use]
    pub const fn depth(self) -> u8 {
        self.0
    }

    /// How far to shift a virtual address to get this level's index.
    #[must_use]
    pub const fn shift(self) -> u32 {
        // Level 0 indexes bits 47..39, and each level down moves nine bits.
        39 - 9 * self.0 as u32
    }

    /// Bytes one descriptor at this level covers.
    #[must_use]
    pub const fn span(self) -> u64 {
        1 << self.shift()
    }

    /// This level's index within a virtual address.
    #[must_use]
    pub const fn index(self, virt: VirtAddr) -> usize {
        ((virt.0 >> self.shift()) & 0x1FF) as usize
    }

    /// The level below, or `None` at the leaves.
    #[must_use]
    pub const fn next(self) -> Option<Level> {
        Level::new(self.0 + 1)
    }
}

/// What a mapping may be used for.
///
/// Deliberately a struct of named booleans rather than a bitflags type: these
/// are written once per call site and read many times, and `MapFlags::KERNEL_
/// CODE` says more at a call site than an integer does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapFlags {
    /// Readable. A mapping that is not readable is not mapped at all on either
    /// architecture, so this exists to make the intent explicit rather than to
    /// be switched off.
    pub read: bool,
    /// Writable.
    pub write: bool,
    /// Executable at the privilege level this mapping is for.
    pub execute: bool,
    /// Reachable from user mode.
    pub user: bool,
    /// Present in every address space, so the translation survives an address
    /// space switch. Kernel mappings only.
    pub global: bool,
    /// Device memory: uncached, and on AArch64 also non-gathering and
    /// non-reordering, which is what an `MMIO` register requires.
    pub device: bool,
    /// Normal memory the caches do not hold: memory shared with a device that
    /// does not snoop them, such as an STM32MP1's USB host reading its
    /// descriptors. Weakly ordered, unlike [`MapFlags::device`], so a writer
    /// still needs a barrier before the device may look. Ignored where
    /// `device` is set, and on x86-64, whose devices all snoop.
    pub uncached: bool,
    /// Write-combining memory: neither cached nor strongly ordered, so the
    /// processor may gather stores into bursts. What a GPU's aperture onto
    /// its own memory is mapped as (`docs/NVIDIA.md` §4.3). Ignored where
    /// `device` is set. On x86-64 it selects the PAT entry the kernel
    /// programs as write-combining ([`X86_PAT`]); on the Arm architectures it
    /// is normal non-cacheable memory, as `uncached` is.
    pub write_combining: bool,
}

/// The x86-64 page attribute table the kernel programs into `IA32_PAT`: the
/// power-on table with entry 1, which a descriptor selects with only its
/// write-through bit, made write-combining (0x01) in place of
/// write-through. Entries 0 and 3 -- write-back and uncacheable, the only
/// ones a descriptor selected before -- are unchanged, and entries 4 to 7,
/// reached through the PAT bit, are left as at power-on.
pub const X86_PAT: u64 = 0x0007_0406_0007_0106;

impl MapFlags {
    /// Kernel text: read and execute, never writable.
    pub const KERNEL_CODE: MapFlags = MapFlags {
        read: true,
        write: false,
        execute: true,
        user: false,
        global: true,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// A page a device may read and write through an IOMMU: a DMA buffer.
    ///
    /// An IOMMU descriptor has no user, global or execute bit, so those are
    /// clear rather than meaningful.
    pub const DMA: MapFlags = MapFlags {
        read: true,
        write: true,
        execute: false,
        user: false,
        global: false,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// A page a device may read but not write through an IOMMU.
    pub const DMA_READ_ONLY: MapFlags = MapFlags {
        read: true,
        write: false,
        execute: false,
        user: false,
        global: false,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// Kernel constants: read only, never executable.
    pub const KERNEL_RODATA: MapFlags = MapFlags {
        read: true,
        write: false,
        execute: false,
        user: false,
        global: true,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// Kernel data, stacks and the direct map: read and write, never
    /// executable. This is the one used for most of the address space, which
    /// is why W^X is its default rather than something switched on later.
    pub const KERNEL_DATA: MapFlags = MapFlags {
        read: true,
        write: true,
        execute: false,
        user: false,
        global: true,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// A device register window.
    pub const KERNEL_DEVICE: MapFlags = MapFlags {
        read: true,
        write: true,
        execute: false,
        user: false,
        global: true,
        device: true,
        uncached: false,
        write_combining: false,
    };

    /// User text.
    pub const USER_CODE: MapFlags = MapFlags {
        read: true,
        write: false,
        execute: true,
        user: true,
        global: false,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// User data and stack.
    pub const USER_DATA: MapFlags = MapFlags {
        read: true,
        write: true,
        execute: false,
        user: true,
        global: false,
        device: false,
        uncached: false,
        write_combining: false,
    };

    /// The same flags with execute permission removed.
    #[must_use]
    pub const fn without_execute(self) -> MapFlags {
        MapFlags {
            execute: false,
            ..self
        }
    }

    /// The same flags with write permission removed.
    #[must_use]
    pub const fn read_only(self) -> MapFlags {
        MapFlags {
            write: false,
            ..self
        }
    }
}

/// The bit layout of one architecture's page table descriptors, and the shape
/// of the walk that reads them.
pub trait Encoding {
    /// A short name, for diagnostics.
    const NAME: &'static str;

    /// The level the hardware walk starts at.
    ///
    /// [`Level::ROOT`] for a four-level walk. A three-level walk starts at
    /// [`Level::GIGABYTE`], and its root table is indexed by the same address
    /// bits as a four-level walk's level-1 table: what differs is only that
    /// there is nothing above it.
    const ROOT_LEVEL: Level = Level::ROOT;

    /// Bits of virtual address the walk translates.
    const VIRT_BITS: u32 = VIRT_BITS;

    /// Bits of physical address a descriptor can hold. An address at or
    /// above `1 << PHYS_BITS` does not fit, and a descriptor that masked it
    /// would map low memory instead.
    const PHYS_BITS: u32 = 48;

    /// True if the hardware can translate `virt` at all.
    ///
    /// The default is the 64-bit rule: every bit from `VIRT_BITS - 1` up must
    /// be the same, which is what makes an upper-half address a sign-extended
    /// one. Both 64-bit architectures fault on an address that breaks it, so
    /// a table built for one is a table with an entry nothing can reach. An
    /// encoding whose addresses are narrower than a `u64` overrides this.
    fn is_canonical(virt: VirtAddr) -> bool {
        let high = virt.0 >> (Self::VIRT_BITS - 1);
        high == 0 || high == u64::MAX >> (Self::VIRT_BITS - 1)
    }

    /// The form of `address`, assembled by a walk from table indices, that a
    /// caller compares against: sign extended, by default.
    ///
    /// A walk builds `0x0000_FF00_...` from indices, and every constant the
    /// kernel compares a mapping against is written `0xFFFF_FF00_...`.
    fn canonical(address: u64) -> u64 {
        let sign = 1u64 << (Self::VIRT_BITS - 1);
        if address & sign == 0 {
            address
        } else {
            address | !((1u64 << Self::VIRT_BITS) - 1)
        }
    }

    /// Descriptor for an intermediate table at `table`.
    ///
    /// `flags` is passed because x86-64 needs the user bit set on every level
    /// of the walk, not only the leaf: an intermediate entry without it blocks
    /// user access to everything beneath.
    fn table_descriptor(table: PhysAddr, flags: MapFlags) -> u64;

    /// Descriptor for a leaf mapping `frame` at `level`.
    fn leaf_descriptor(frame: PhysAddr, level: Level, flags: MapFlags) -> u64;

    /// True if the descriptor maps or points at anything.
    fn is_present(entry: u64) -> bool;

    /// True if the descriptor is a leaf rather than a pointer to a table.
    fn is_leaf(entry: u64, level: Level) -> bool;

    /// The address a descriptor holds, whether leaf or table.
    fn address(entry: u64) -> PhysAddr;

    /// True if a block mapping is architecturally allowed at `level`.
    fn supports_block(level: Level) -> bool;

    /// What a leaf descriptor permits.
    ///
    /// The inverse of [`Encoding::leaf_descriptor`], and it exists for one
    /// reason: a sweep that walks the live tables and asserts no mapping is
    /// both writable and executable has to read permissions back out of
    /// descriptors that were written by the loader, not by this crate. A
    /// round-trip test for every flag combination is in `tests`, because a
    /// decoder that disagrees with the encoder would make the sweep pass by
    /// being wrong.
    fn leaf_flags(entry: u64) -> MapFlags;

    /// The descriptor value that means "nothing is mapped here".
    ///
    /// Zero on every architecture here, and named rather than written as a
    /// literal so that an architecture whose absent encoding is not zero has
    /// somewhere to say so.
    const ABSENT: u64 = 0;
}

/// Access to physical memory, and a source of zeroed page table frames.
///
/// The mapper does no `unsafe` of its own: everything it needs to touch goes
/// through this trait, so the one place that has to be reasoned about is the
/// implementation. A test implements it over a map; the loader implements it
/// over the identity mapping firmware left in place; the kernel implements it
/// over the direct map.
///
/// # Safety
///
/// An implementation must guarantee that:
///
/// * `read` and `write` address real, mapped, naturally aligned physical
///   memory, and that no other reference aliases it;
/// * `allocate_table` returns a 4 KiB-aligned frame of `PAGE_SIZE` bytes,
///   filled with zeroes, that is not in use for anything else and stays valid
///   for as long as the table it becomes part of.
pub unsafe trait PhysMem {
    /// Read the eight bytes at `at`.
    fn read(&self, at: PhysAddr) -> u64;

    /// Write eight bytes at `at`.
    fn write(&mut self, at: PhysAddr, value: u64);

    /// Take a zeroed 4 KiB frame to use as a page table.
    fn allocate_table(&mut self) -> Option<PhysAddr>;
}

/// Why a mapping could not be established.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// The virtual address, physical address or length was not page aligned.
    Misaligned,
    /// The virtual address is not one the encoding's walk can translate: not
    /// canonical on a 48-bit geometry, above 4 GiB on a 32-bit one.
    NotCanonical,
    /// No frame was available for an intermediate table.
    OutOfMemory,
    /// Something is already mapped in the requested range. Carries the address.
    AlreadyMapped(VirtAddr),
    /// The walk met a block mapping where it needed a table, so the request
    /// would have had to split an existing large page. Carries the address.
    BlockInTheWay(VirtAddr),
    /// The range wraps the end of the address space.
    RangeOverflow,
    /// The physical range reaches past what the encoding's descriptors can
    /// hold.
    PhysicalOutOfRange,
    /// A table write that a walker which does not snoop the caches reads was
    /// not cleaned to memory before it was published
    /// ([`coherence::Unpublished`]), so the change is refused.
    NotCleaned,
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::Misaligned => f.write_str("address or length is not page aligned"),
            MapError::NotCanonical => f.write_str("virtual address is not canonical"),
            MapError::OutOfMemory => f.write_str("no frame available for a page table"),
            MapError::AlreadyMapped(at) => write!(f, "{at:?} is already mapped"),
            MapError::BlockInTheWay(at) => write!(f, "{at:?} is inside a larger block mapping"),
            MapError::RangeOverflow => f.write_str("range wraps the address space"),
            MapError::PhysicalOutOfRange => {
                f.write_str("physical address is wider than a descriptor can hold")
            }
            MapError::NotCleaned => {
                f.write_str("a table write was not cleaned to memory before it was published")
            }
        }
    }
}

/// Builds and inspects a page table tree.
#[derive(Debug)]
pub struct Mapper<E: Encoding> {
    root: PhysAddr,
    largest_block: Level,
    encoding: PhantomData<E>,
}

impl<E: Encoding> Mapper<E> {
    /// Wrap an existing root table.
    ///
    /// The default block size is 2 MiB, because 1 GiB pages are an optional
    /// x86-64 feature (`CPUID.80000001H:EDX.PDPE1GB`) and a table that uses
    /// one on a CPU without it faults on first touch. Call
    /// [`Mapper::allow_gigabyte_blocks`] once that has been checked.
    #[must_use]
    pub const fn new(root: PhysAddr) -> Self {
        Mapper {
            root,
            largest_block: Level::MEGABYTE,
            encoding: PhantomData,
        }
    }

    /// The physical address of the root table, for `CR3` or `TTBR`.
    #[must_use]
    pub const fn root(&self) -> PhysAddr {
        self.root
    }

    /// Permit 1 GiB block mappings, which makes the direct map of physical
    /// memory a few hundred descriptors instead of a few hundred thousand.
    pub const fn allow_gigabyte_blocks(&mut self) {
        self.largest_block = Level::GIGABYTE;
    }

    /// Map 4 KiB pages only, never a block: for a table whose walker may not
    /// take one, such as a VT-d unit whose capabilities report no 2 MiB pages.
    pub const fn pages_only(&mut self) {
        self.largest_block = Level::PAGE;
    }

    /// Map `len` bytes at `virt` onto `phys`.
    ///
    /// Uses the largest blocks alignment allows, so mapping the whole of
    /// physical memory is cheap. Fails rather than overwriting anything
    /// already mapped — a loader that silently replaces a mapping is a loader
    /// whose bugs appear after the jump.
    pub fn map_range(
        &self,
        memory: &mut impl PhysMem,
        virt: VirtAddr,
        phys: PhysAddr,
        len: u64,
        flags: MapFlags,
    ) -> Result<(), MapError> {
        if !virt.is_aligned_to(PAGE_SIZE) || !phys.is_aligned_to(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }
        if !len.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }
        if !E::is_canonical(virt) {
            return Err(MapError::NotCanonical);
        }
        // The whole range, not only its start, and before anything is written.
        // A walk indexes each level from a few bits of the address and ignores
        // the rest, so a page past the canonical boundary does not fail: it
        // lands in another root slot and aliases an address somebody else owns.
        if let Some(last_offset) = len.checked_sub(1) {
            let last = virt
                .0
                .checked_add(last_offset)
                .ok_or(MapError::RangeOverflow)?;
            // The first address above the lower half. On the 64-bit pair it
            // opens the gap between the halves, and a range from one half to
            // the other runs through it with both ends canonical; on ARMv7-A
            // the halves touch, and it is an address like any other.
            let boundary = 1u64 << (E::VIRT_BITS - 1);
            let crosses = virt.0 < boundary && last >= boundary;
            if !E::is_canonical(VirtAddr(last)) || (crosses && !E::is_canonical(VirtAddr(boundary)))
            {
                return Err(MapError::NotCanonical);
            }
        }
        let limit = 1_u64.checked_shl(E::PHYS_BITS).unwrap_or(u64::MAX);
        if phys.0.checked_add(len).is_none_or(|end| end > limit) {
            return Err(MapError::PhysicalOutOfRange);
        }

        let mut done = 0u64;
        while done < len {
            let at = virt.checked_add(done).ok_or(MapError::RangeOverflow)?;
            let frame = phys.checked_add(done).ok_or(MapError::RangeOverflow)?;
            let remaining = len - done;

            let level = self.choose_level(at, frame, remaining);
            self.map_one(memory, at, frame, level, flags)?;
            done += level.span();
        }

        Ok(())
    }

    /// The largest level whose block fits this address, frame and remainder.
    fn choose_level(&self, virt: VirtAddr, phys: PhysAddr, remaining: u64) -> Level {
        for depth in self.largest_block.depth()..Level::PAGE.depth() {
            let Some(level) = Level::new(depth) else {
                continue;
            };
            if !E::supports_block(level) {
                continue;
            }
            let span = level.span();
            if virt.is_aligned_to(span) && phys.is_aligned_to(span) && remaining >= span {
                return level;
            }
        }
        Level::PAGE
    }

    /// Install one leaf descriptor, creating the tables above it.
    fn map_one(
        &self,
        memory: &mut impl PhysMem,
        virt: VirtAddr,
        phys: PhysAddr,
        target: Level,
        flags: MapFlags,
    ) -> Result<(), MapError> {
        let mut table = self.root;
        let mut level = E::ROOT_LEVEL;

        while level < target {
            let slot = descriptor_address(table, level.index(virt));
            let entry = memory.read(slot);

            table = if E::is_present(entry) {
                if E::is_leaf(entry, level) {
                    return Err(MapError::BlockInTheWay(virt));
                }
                E::address(entry)
            } else {
                let fresh = memory.allocate_table().ok_or(MapError::OutOfMemory)?;
                memory.write(slot, E::table_descriptor(fresh, flags));
                fresh
            };

            level = level.next().ok_or(MapError::BlockInTheWay(virt))?;
        }

        let slot = descriptor_address(table, target.index(virt));
        if E::is_present(memory.read(slot)) {
            return Err(MapError::AlreadyMapped(virt));
        }
        memory.write(slot, E::leaf_descriptor(phys, target, flags));
        Ok(())
    }

    /// Resolve `virt` the way the hardware would, or `None` if it is unmapped.
    pub fn translate(&self, memory: &impl PhysMem, virt: VirtAddr) -> Option<PhysAddr> {
        let mut table = self.root;
        let mut level = E::ROOT_LEVEL;

        loop {
            let entry = memory.read(descriptor_address(table, level.index(virt)));
            if !E::is_present(entry) {
                return None;
            }
            if E::is_leaf(entry, level) {
                let offset = virt.0 & (level.span() - 1);
                return E::address(entry).checked_add(offset);
            }
            table = E::address(entry);
            level = level.next()?;
        }
    }

    /// The flags of the leaf that maps `virt`, as [`Encoding::leaf_flags`]
    /// reads them back, or `None` if it is unmapped: how a check sees the
    /// memory type a mapping was given.
    pub fn leaf_flags(&self, memory: &impl PhysMem, virt: VirtAddr) -> Option<MapFlags> {
        let mut table = self.root;
        let mut level = E::ROOT_LEVEL;

        loop {
            let entry = memory.read(descriptor_address(table, level.index(virt)));
            if !E::is_present(entry) {
                return None;
            }
            if E::is_leaf(entry, level) {
                return Some(E::leaf_flags(entry));
            }
            table = E::address(entry);
            level = level.next()?;
        }
    }

    /// Remove `len` bytes of mapping at `virt`, reporting what came out.
    ///
    /// `released` is called once per frame the tree stopped referring to,
    /// which is not only the leaves: an intermediate table whose last
    /// descriptor has just been cleared is reported too, and the levels above
    /// it in turn if they empty out as well. Without that a long-lived arena
    /// leaks a table per region it ever touched — the pages come back and the
    /// tables that described them do not — and the leak is invisible, because
    /// the free-page count balances perfectly.
    ///
    /// The mapper frees nothing itself. It has no idea whether the frame
    /// behind a mapping is anonymous memory to give back, a device aperture
    /// that was never allocated, or a page shared with another address space.
    /// That decision belongs to the caller, and this is how it learns what to
    /// decide about.
    ///
    /// # Errors
    ///
    /// [`MapError::Misaligned`] for an address or length that is not page
    /// aligned, and [`MapError::BlockInTheWay`] if the range would have to cut
    /// a block mapping in half — unmapping half a 2 MiB page means splitting
    /// it, and silently unmapping the whole thing instead would hand back
    /// memory the caller still believes it owns.
    pub fn unmap_range(
        &self,
        memory: &mut impl PhysMem,
        virt: VirtAddr,
        len: u64,
        mut released: impl FnMut(Released),
    ) -> Result<u64, MapError> {
        if !virt.is_aligned_to(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }

        let mut done = 0u64;
        let mut removed = 0u64;
        while done < len {
            let at = virt.checked_add(done).ok_or(MapError::RangeOverflow)?;
            let mut path = Path::default();
            match self.find_leaf(memory, at, Some(&mut path))? {
                // Nothing mapped: carry on past the hole. Unmapping one is not
                // an error — freeing a `vmap` allocation walks straight across
                // the guard page inside its own span. And the walk stopped at
                // an absent descriptor, so nothing is mapped anywhere in that
                // descriptor's span: the hole is stepped over whole. A page
                // at a time, a program handing back a reservation of
                // gigabytes it never touched -- a browser's allocator does it
                // all day -- cost a walk from the root per page of it.
                Found::Hole(level) => {
                    let span = level.span();
                    let next = (at.0 | (span - 1)).checked_add(1);
                    done = next.map_or(len, |next| (next - virt.0).min(len));
                }
                Found::Leaf(slot, entry, level) => {
                    let span = level.span();
                    if !at.is_aligned_to(span) || len - done < span {
                        return Err(MapError::BlockInTheWay(at));
                    }
                    memory.write(slot, E::ABSENT);
                    released(Released::Page {
                        phys: E::address(entry),
                        level,
                    });
                    self.prune(memory, &path, &mut released);
                    removed += 1;
                    done += span;
                }
            }
        }
        Ok(removed)
    }

    /// Unlink every table in the `len` bytes at `virt` that maps nothing, and
    /// report each through `released` as [`Mapper::unmap_range`] reports a
    /// table it emptied: deepest first, the root never.
    ///
    /// A table empties only when a leaf under it is removed, which is when
    /// [`Mapper::unmap_range`] prunes it. A [`Mapper::map_range`] that ran out
    /// of memory part-way leaves the tables it made above the leaf it could
    /// not install, holding nothing, and no removal will ever prune them.
    /// They are harmless while the tree lives -- the next mapping there reuses
    /// them -- and this is how whoever tears the tree down finds them.
    /// Nothing mapped is touched.
    ///
    /// # Errors
    ///
    /// [`MapError::Misaligned`] for an address or length that is not page
    /// aligned.
    pub fn prune_range(
        &self,
        memory: &mut impl PhysMem,
        virt: VirtAddr,
        len: u64,
        mut released: impl FnMut(Released),
    ) -> Result<(), MapError> {
        if !virt.is_aligned_to(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }
        let mut done = 0u64;
        while done < len {
            let at = virt.checked_add(done).ok_or(MapError::RangeOverflow)?;
            let mut path = Path::default();
            let span = match self.find_leaf(memory, at, Some(&mut path))? {
                Found::Hole(level) => {
                    self.prune(memory, &path, &mut released);
                    level.span()
                }
                Found::Leaf(_, _, level) => level.span(),
            };
            let next = (at.0 | (span - 1)).checked_add(1);
            done = next.map_or(len, |next| (next - virt.0).min(len));
        }
        Ok(())
    }

    /// Free every table on `path` that the leaf's removal has just emptied.
    ///
    /// Walks from the leaf's own table upwards and stops at the first table
    /// with anything left in it — there is no point looking higher, since a
    /// parent still holds the descriptor for the table that is not empty. The
    /// root is never freed: it is not ours, it belongs to whoever installed
    /// `CR3` or `TTBR1`.
    fn prune(&self, memory: &mut impl PhysMem, path: &Path, released: &mut impl FnMut(Released)) {
        for step in path.steps().rev() {
            if !is_empty::<E>(memory, step.table) {
                return;
            }
            memory.write(descriptor_address(step.parent, step.slot), E::ABSENT);
            released(Released::Table { phys: step.table });
        }
    }

    /// Change the permissions of an existing mapping, leaving it in place.
    ///
    /// What `mprotect` needs, and what the loader's identity map would need if
    /// it were being narrowed rather than dropped. Every page in the range has
    /// to be mapped already: a hole means the caller's idea of the range and
    /// the tables' disagree, and guessing which is right is how a W^X sweep
    /// ends up protecting an address nobody mapped.
    ///
    /// # Errors
    ///
    /// [`MapError::Misaligned`], [`MapError::BlockInTheWay`] if the range cuts
    /// a block, and [`MapError::AlreadyMapped`] — reused to mean its opposite,
    /// "expected a mapping and found none", which is the one place in this
    /// crate the name reads badly and is not worth a variant of its own.
    pub fn protect_range(
        &self,
        memory: &mut impl PhysMem,
        virt: VirtAddr,
        len: u64,
        flags: MapFlags,
    ) -> Result<(), MapError> {
        if !virt.is_aligned_to(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }

        let mut done = 0u64;
        while done < len {
            let at = virt.checked_add(done).ok_or(MapError::RangeOverflow)?;
            let Found::Leaf(slot, entry, level) = self.find_leaf(memory, at, None)? else {
                return Err(MapError::AlreadyMapped(at));
            };
            let span = level.span();
            if !at.is_aligned_to(span) || len - done < span {
                return Err(MapError::BlockInTheWay(at));
            }
            memory.write(slot, E::leaf_descriptor(E::address(entry), level, flags));
            done += span;
        }
        Ok(())
    }

    /// Visit every leaf in the tree, in address order.
    ///
    /// The W^X sweep is the caller this was written for: it needs every
    /// mapping the hardware can see, including the ones the loader installed,
    /// and there is no list of those anywhere but the tables themselves.
    ///
    /// `visit` returns `true` to carry on and `false` to stop, so a sweep that
    /// has found what it was looking for does not have to walk the direct map
    /// to the end.
    pub fn for_each_leaf(
        &self,
        memory: &impl PhysMem,
        mut visit: impl FnMut(Leaf) -> bool,
    ) -> WalkOutcome {
        let mut state = Walk {
            memory,
            visit: &mut visit,
            leaves: 0,
        };
        let stopped = !state.descend::<E>(self.root, E::ROOT_LEVEL, 0);
        WalkOutcome {
            leaves: state.leaves,
            stopped,
        }
    }

    /// The descriptor slot, value and level of whatever maps `virt`, or the
    /// level of the absent descriptor that says nothing does.
    ///
    /// When `path` is given it is filled with the intermediate tables the walk
    /// descended through, which is what [`Mapper::prune`] needs: a table knows
    /// nothing about its own parent, so the only way to clear the descriptor
    /// pointing at an emptied table is to have remembered where it was.
    fn find_leaf(
        &self,
        memory: &impl PhysMem,
        virt: VirtAddr,
        mut path: Option<&mut Path>,
    ) -> Result<Found, MapError> {
        let mut table = self.root;
        let mut level = E::ROOT_LEVEL;

        loop {
            let index = level.index(virt);
            let slot = descriptor_address(table, index);
            let entry = memory.read(slot);
            if !E::is_present(entry) {
                return Ok(Found::Hole(level));
            }
            if E::is_leaf(entry, level) {
                return Ok(Found::Leaf(slot, entry, level));
            }
            if let Some(path) = path.as_deref_mut() {
                path.push(Step {
                    parent: table,
                    slot: index,
                    table: E::address(entry),
                });
            }
            table = E::address(entry);
            level = level.next().ok_or(MapError::BlockInTheWay(virt))?;
        }
    }

    /// The level at which `virt` is mapped, for tests and diagnostics.
    pub fn mapping_level(&self, memory: &impl PhysMem, virt: VirtAddr) -> Option<Level> {
        let mut table = self.root;
        let mut level = E::ROOT_LEVEL;

        loop {
            let entry = memory.read(descriptor_address(table, level.index(virt)));
            if !E::is_present(entry) {
                return None;
            }
            if E::is_leaf(entry, level) {
                return Some(level);
            }
            table = E::address(entry);
            level = level.next()?;
        }
    }
}

/// What [`Mapper::find_leaf`] found at an address.
enum Found {
    /// The descriptor slot, value and level of the leaf that maps it.
    Leaf(PhysAddr, u64, Level),
    /// Nothing: the walk met an absent descriptor at this level, so nothing
    /// is mapped anywhere in that level's span around the address.
    Hole(Level),
}

/// A frame the tree has stopped referring to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Released {
    /// A mapping was removed. The frame behind it may be anonymous memory, a
    /// device aperture or a page shared with another address space, and only
    /// the caller knows which.
    Page {
        /// What it mapped to.
        phys: PhysAddr,
        /// The level it was mapped at, and so how big it was.
        level: Level,
    },
    /// An intermediate table emptied out and was unlinked from its parent.
    /// Always one 4 KiB frame, and always one the mapper itself allocated
    /// through [`PhysMem::allocate_table`].
    Table {
        /// The frame the table occupied.
        phys: PhysAddr,
    },
}

/// One level of a walk from the root to a leaf.
#[derive(Clone, Copy, Default, Debug)]
struct Step {
    /// The table holding the descriptor that points at `table`.
    parent: PhysAddr,
    /// Which of the parent's 512 descriptors that is.
    slot: usize,
    /// The table it points at.
    table: PhysAddr,
}

/// The intermediate tables a walk descended through, root first.
///
/// A fixed array rather than a `Vec`: this crate allocates nothing, and no
/// geometry here has more than four levels, so the bound is architectural
/// rather than a guess. The root is not a step — nothing points at it — which
/// is why there are at most three, and why a three-level walk uses two.
#[derive(Clone, Copy, Default, Debug)]
struct Path {
    steps: [Step; 3],
    depth: usize,
}

impl Path {
    /// Record one level of descent. Silently ignores anything past the fourth
    /// level, which no geometry here can produce and would mean a walk that
    /// had lost its way if it did.
    fn push(&mut self, step: Step) {
        if let Some(slot) = self.steps.get_mut(self.depth) {
            *slot = step;
            self.depth += 1;
        }
    }

    /// The steps actually taken, root first.
    fn steps(&self) -> impl DoubleEndedIterator<Item = &Step> {
        self.steps.get(..self.depth).unwrap_or(&[]).iter()
    }
}

/// True if every descriptor in `table` is absent.
fn is_empty<E: Encoding>(memory: &impl PhysMem, table: PhysAddr) -> bool {
    (0..ENTRIES).all(|index| !E::is_present(memory.read(descriptor_address(table, index))))
}

/// One mapping found by [`Mapper::for_each_leaf`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Leaf {
    /// Where it starts in the virtual address space.
    pub virt: VirtAddr,
    /// What it maps to.
    pub phys: PhysAddr,
    /// The level it is mapped at, and so how big it is.
    pub level: Level,
    /// What it permits, decoded from the descriptor.
    pub flags: MapFlags,
}

impl Leaf {
    /// How many bytes this mapping covers.
    ///
    /// Named `bytes` rather than `len` because a leaf always covers at least a
    /// page: there is no empty mapping for the `is_empty` that `len` would owe.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.level.span()
    }

    /// True if this mapping is both writable and executable.
    ///
    /// The thing a W^X sweep is looking for, spelled once here so that the
    /// kernel's sweep and this crate's tests cannot drift apart about what the
    /// rule is.
    #[must_use]
    pub const fn is_write_execute(&self) -> bool {
        self.flags.write && self.flags.execute
    }
}

/// What a walk found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WalkOutcome {
    /// Leaves visited.
    pub leaves: u64,
    /// True if the visitor asked to stop before the end.
    pub stopped: bool,
}

/// The recursion state of [`Mapper::for_each_leaf`].
///
/// A struct rather than four arguments threaded through a free function,
/// because the visitor has to be borrowed mutably across the recursion and a
/// closure cannot be passed by value into itself.
struct Walk<'a, M: PhysMem + ?Sized, F: FnMut(Leaf) -> bool> {
    memory: &'a M,
    visit: &'a mut F,
    leaves: u64,
}

impl<M: PhysMem + ?Sized, F: FnMut(Leaf) -> bool> Walk<'_, M, F> {
    /// Walk the table at `table`, whose entries cover addresses from `base`.
    ///
    /// Returns `false` as soon as the visitor asks to stop, which unwinds the
    /// whole recursion rather than only the current table.
    fn descend<E: Encoding>(&mut self, table: PhysAddr, level: Level, base: u64) -> bool {
        for index in 0..ENTRIES {
            let entry = self.memory.read(descriptor_address(table, index));
            if !E::is_present(entry) {
                continue;
            }

            // The walk builds an address from table indices, which yields the
            // unextended form; a kernel mapping reported as `0x0000_FF00_...`
            // rather than `0xFFFF_FF00_...` would be a correct walk of the
            // wrong-looking address, and every caller compares it against a
            // constant. The encoding knows which form its constants are in.
            let virt = VirtAddr(E::canonical(base | ((index as u64) << level.shift())));

            if E::is_leaf(entry, level) {
                self.leaves += 1;
                let leaf = Leaf {
                    virt,
                    phys: E::address(entry),
                    level,
                    flags: E::leaf_flags(entry),
                };
                if !(self.visit)(leaf) {
                    return false;
                }
                continue;
            }

            let Some(below) = level.next() else {
                continue;
            };
            if !self.descend::<E>(E::address(entry), below, virt.0 & !(level.span() - 1)) {
                return false;
            }
        }
        true
    }
}

/// Physical address of descriptor `index` in the table at `table`.
const fn descriptor_address(table: PhysAddr, index: usize) -> PhysAddr {
    PhysAddr(table.0 + (index as u64) * 8)
}

#[cfg(test)]
mod tests;
