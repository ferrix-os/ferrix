//! Stage 10's device self-check: play driver for a virtio entropy device,
//! once, before any real driver exists.
//!
//! The kernel does not drive devices (`docs/ARCHITECTURE.md` §7). What it
//! does own is everything underneath a driver: the BAR mappings, the
//! capability locations, and the physical memory a device reads and writes.
//! None of that is proven by enumeration, which only reads configuration
//! space. So once, at boot, the kernel takes the part of a driver for the
//! simplest device there is — virtio-rng, one queue, no configuration — and
//! requires that a device given addresses in memory the kernel allocated
//! writes into exactly that memory. Then it resets the device and gives
//! everything back.
//!
//! # What halts the boot, and what does not
//!
//! A completion that cannot be right — naming a request nobody made, claiming
//! a length the device was not given, or leaving the bytes it claims to have
//! written untouched — halts the boot everywhere, because it is the broken
//! DMA path this check exists to find.
//!
//! A device that merely refuses, stalls or will not reset is reported and
//! skipped. On the machines `xtask` boots that would still be a regression,
//! and `xtask test-boot` fails the boot that reads no entropy. But Ferrix does
//! not only boot where its own tools configured the hypervisor: libvirt adds a
//! virtio-rng to every guest it defines, and one backed by a rate-limited or
//! drained entropy source can miss any deadline chosen here without anything
//! in the kernel being wrong.
//!
//! # By interrupt
//!
//! Where the device has MSI-X, the completion is not polled for: table entry 0
//! is programmed with a vector the architecture allocated, the queue is told
//! to use it, and the used ring is read only once the vector has been
//! delivered. So the check proves the message path — table, controller,
//! vector, dispatch — as well as DMA. A completion that arrives without its
//! interrupt is reported and skipped, like a stall.
//!
//! It is also the harness the next piece of stage 10 needs: an IOMMU domain
//! is proven when a descriptor pointing outside it faults.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;
use ferrix_pci::bar::{Bar, Region};
use ferrix_pci::capability::{Capability, MSIX_ENTRY_SIZE, MsiX};
use ferrix_pci::header::{COMMAND, COMMAND_BUS_MASTER, COMMAND_MEMORY_SPACE};
use ferrix_pci::msix::{
    self, CAPABILITY_CONTROL, CONTROL_ENABLE, CONTROL_FUNCTION_MASK, ENTRY_ADDRESS_HIGH,
    ENTRY_ADDRESS_LOW, ENTRY_DATA, ENTRY_VECTOR_CONTROL, VECTOR_CONTROL_MASKED,
};
use ferrix_pci::virtio::{Location, Transport};
use ferrix_pci::{Address, ConfigSpace};
use ferrix_virtio::pci::{
    self as transport, COMMON_CONFIG_LEN, CommonConfig, NO_VECTOR, QueueAddresses, TransportError,
};
use ferrix_virtio::{Buffer, Layout, QueueMemory, SplitQueue};

use super::{Failure, Space};
use crate::arch;
use crate::device::Reserved;
use crate::iommu;
use crate::irq::{self, Msi};
use crate::mm;
use crate::mmio::Mmio;
use crate::sync::SpinLock;
use crate::timer;
use crate::vmap;

/// Entries in the queue. Eight is ample for one request, and small enough
/// that the rings fit in one page.
const QUEUE_SIZE: u16 = 8;

/// Bytes of entropy asked for. A device may write fewer: virtio lets an
/// entropy device use less than the whole buffer.
const REQUEST: u32 = 64;

/// Bytes below which an all-zero completion is not evidence of anything: one
/// correct byte is zero one time in 256.
const ZERO_EVIDENCE: u32 = 8;

/// Reads of `device_status` a reset may take.
const RESET_POLLS: u32 = 100_000;

/// How long the device has to complete the request.
const DEADLINE_NANOS: u64 = 2_000_000_000;

/// What the check found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Entropy {
    /// The device wrote `bytes` where it was told to.
    Read {
        /// How many.
        bytes: u32,
        /// `None` if the completion arrived by MSI-X; otherwise why it was
        /// polled for.
        polled: Option<&'static str>,
        /// On a translated domain, whether a write outside it faulted: `Ok`
        /// when the unit recorded it, otherwise why it was not shown. `None`
        /// when no unit translates the device.
        out_of_domain: Option<Result<Faulted, &'static str>>,
    },
    /// The device refused or stalled, for this reason, and was left alone.
    Skipped(&'static str),
}

/// A write outside the domain, which the unit recorded as a fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Faulted {
    /// The completion the device reported for it anyway, if it did.
    ///
    /// QEMU's does. Its DMA map of an address the IOMMU refuses does not
    /// fail: `address_space_map` hands the device a bounce buffer, the device
    /// fills it and completes the request with the length it was given, and
    /// the write-back on unmap is refused a second time and dropped. Nothing
    /// reaches the page, which is what the unit's record says and what the
    /// check requires; the completion is a fact about the device model.
    pub(crate) completed: Option<Completed>,
}

/// A completion the device reported for a write its unit faulted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Completed {
    /// The length the device said it wrote.
    pub(crate) written: u32,
    /// Whether the check saw the completion before it saw the fault. The unit
    /// records the fault first, but the check reads the two from different
    /// places, so it can read the used ring after the device's push and the
    /// event queue from before it.
    pub(crate) before_fault: bool,
    /// Faults for the probe page recorded after the first: the dropped
    /// write-back is one.
    pub(crate) further_faults: u32,
}

/// One register block from a BAR, mapped for as long as this lives.
#[derive(Debug)]
struct Block {
    /// The registers.
    registers: Mmio,
    /// Where `vmap` put them, to give back.
    virt: u64,
    /// Bytes long.
    len: u64,
}

impl Block {
    /// Where the block `location` names is, in the memory BAR `regions` holds
    /// for it, as a physical address and a length.
    ///
    /// `Transport::verify` has already required the block to lie inside a
    /// memory BAR; the BAR kind is checked again here because the address of
    /// an I/O BAR is a port number, and mapping it as physical memory would
    /// write the device's registers into whatever RAM that number names.
    fn locate(location: Location, regions: &[Region]) -> Result<(u64, u64), &'static str> {
        let region = regions
            .iter()
            .find(|region| region.index == location.bar)
            .ok_or("a virtio block names a BAR that was not sized")?;
        let Bar::Memory { address, .. } = region.bar else {
            return Err("a virtio block is in an I/O BAR");
        };
        let phys = address
            .checked_add(u64::from(location.offset))
            .ok_or("a virtio block's address overflows")?;
        Ok((phys, u64::from(location.length)))
    }

    /// Map `len` bytes of registers at `phys`.
    fn map(phys: u64, len: u64) -> Result<Self, &'static str> {
        let virt = vmap::map_device(phys, len).map_err(|_| "a virtio block could not be mapped")?;
        Ok(Block {
            registers: Mmio::at(virt),
            virt,
            len,
        })
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        let _ = vmap::unmap_device(self.virt);
    }
}

/// The common configuration, through its mapped block.
#[derive(Debug)]
struct Common<'b>(&'b Block);

impl CommonConfig for Common<'_> {
    fn read8(&self, offset: u32) -> u8 {
        self.0.registers.read8(u64::from(offset))
    }
    fn read16(&self, offset: u32) -> u16 {
        self.0.registers.read16(u64::from(offset))
    }
    fn read32(&self, offset: u32) -> u32 {
        self.0.registers.read32(u64::from(offset))
    }
    fn write8(&mut self, offset: u32, value: u8) {
        self.0.registers.write8(u64::from(offset), value);
    }
    fn write16(&mut self, offset: u32, value: u16) {
        self.0.registers.write16(u64::from(offset), value);
    }
    fn write32(&mut self, offset: u32, value: u32) {
        self.0.registers.write32(u64::from(offset), value);
    }
}

/// One zeroed page a device is given the physical address of.
#[derive(Debug)]
struct DmaPage {
    /// The frame number.
    frame: u64,
}

impl DmaPage {
    /// Take and zero a frame.
    fn new() -> Option<Self> {
        let frame = mm::allocate_frames(0)?;
        mm::zero_frame(frame);
        Some(DmaPage { frame })
    }

    /// The physical address the device is given.
    const fn phys(&self) -> u64 {
        self.frame * PAGE_SIZE
    }

    /// Where the kernel reads and writes the same page.
    fn virt(&self) -> u64 {
        mm::direct_map(self.phys())
    }

    /// Keep the frame out of the allocator for good, because a device that
    /// would not reset may still hold its address and write to it.
    fn leak(self) {
        let _ = core::mem::ManuallyDrop::new(self);
    }
}

impl Drop for DmaPage {
    fn drop(&mut self) {
        mm::deallocate_frames(self.frame, 0);
    }
}

/// The rings, in a [`DmaPage`] that outlives the queue built over them.
#[derive(Debug)]
struct Rings {
    /// The page's direct-map address.
    virt: u64,
}

// SAFETY: (DMA) `virt` is the direct-map alias of a whole page taken for this queue
// alone and held until after the device has been reset: at least
// `Layout::total_size` bytes (`drive` checks the layout fits a page), aligned to
// a page and so to 16 bytes, and shared with nothing but the device. Each `u16`
// is one aligned access, as the trait asks of the ring's indices. `barrier` is
// `arch::dma_barrier`, which orders the accesses as the device sees them: a
// volatile access only stops the compiler, and an Arm core reorders (F-44).
unsafe impl QueueMemory for Rings {
    fn read_u8(&self, offset: usize) -> u8 {
        // SAFETY: (DMA) `offset` is below `Layout::total_size`, which fits in the
        // page, so the address is inside memory this queue owns.
        unsafe { core::ptr::read_volatile((self.virt + offset as u64) as *const u8) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        // SAFETY: (DMA) as `read_u8`.
        unsafe { core::ptr::write_volatile((self.virt + offset as u64) as *mut u8, value) };
    }

    fn read_u16(&self, offset: usize) -> u16 {
        debug_assert_eq!(offset % 2, 0, "a ring u16 at an odd offset");
        // SAFETY: (DMA) as `read_u8`, and aligned: every offset `SplitQueue`
        // passes here is one of `Layout`'s field offsets, which are all even
        // (`every_shared_field_is_even`), in a page-aligned page.
        u16::from_le(unsafe { core::ptr::read_volatile((self.virt + offset as u64) as *const u16) })
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        debug_assert_eq!(offset % 2, 0, "a ring u16 at an odd offset");
        // SAFETY: (DMA) as `read_u16`: in the page, and even by `Layout`.
        unsafe {
            core::ptr::write_volatile((self.virt + offset as u64) as *mut u16, value.to_le());
        };
    }

    fn barrier(&self) {
        arch::dma_barrier();
    }
}

/// Tell the device its queue has something new.
///
/// The index the chain was published under is a store to memory and the
/// doorbell a store to a register, which an Arm core does not keep in order
/// unless told: without the barrier the device can look before it can see
/// (F-44).
fn ring_doorbell(notify: &Block, doorbell: u64) {
    arch::dma_barrier();
    notify.registers.write16(doorbell, 0);
}

/// Interrupts the check's vector has delivered.
static DELIVERED: AtomicU64 = AtomicU64::new(0);

/// Diagnostic: the processor the vector was allocated on.
static DIAG_ALLOC_CPU: AtomicU64 = AtomicU64::new(99);

/// The check's interrupt handler: count the delivery, and nothing else.
fn on_entropy(_number: u32) {
    let _ = DELIVERED.fetch_add(1, Ordering::SeqCst);
}

/// The message vectors the check uses, by the requester ID of the device each
/// is for: allocated and registered the first time that device is checked
/// and kept for the life of the machine, since `irq` cannot take a handler
/// back out, so a vector given back here could not be reused anyway.
///
/// One per device rather than one for all: a GICv3's ITS translates a message
/// by the device that wrote it, and a vector mapped for one device is
/// nothing when another writes the same message.
static VECTORS: SpinLock<Vec<(u32, Msi)>> = SpinLock::new(Vec::new());

/// The check's vector for the device at `address`, or why there is none.
fn vector(address: Address) -> Result<Msi, &'static str> {
    let requester = u32::from(address.requester_id());
    let known = VECTORS
        .lock()
        .iter()
        .find(|(device, _)| *device == requester)
        .map(|&(_, msi)| msi);
    if let Some(msi) = known {
        return Ok(msi);
    }
    // Enumeration checks one function at a time, so nothing else is minting
    // this device's vector between the look above and the push below. Room
    // for it is made first: `irq` cannot take a handler back out.
    crate::fallible::try_reserve(&mut VECTORS.lock(), 1)
        .map_err(|_| "no memory to record the check's MSI vector")?;
    let msi = arch::msi_allocate(requester)?;
    DIAG_ALLOC_CPU.store(
        crate::smp::this_cpu().map_or(99, |cpu| cpu.logical) as u64,
        Ordering::SeqCst,
    );
    irq::register(msi.number, on_entropy).map_err(|_| "the MSI vector already has a handler")?;
    let _ = crate::fallible::push_within(&mut VECTORS.lock(), (requester, msi));
    Ok(msi)
}

/// MSI-X table entry 0, programmed and enabled, and what to put back.
#[derive(Debug)]
struct Delivery {
    /// The function's MSI-X capability.
    capability: Capability,
    /// The entry's sixteen bytes, mapped.
    entry: Block,
    /// The capability's control word before the check enabled MSI-X.
    control: u16,
}

impl Delivery {
    /// Program table entry 0 with the check's vector, unmask it, and enable
    /// MSI-X. Needs memory decoding on, since the table is in a BAR.
    ///
    /// The entry is vetted like the virtio blocks: in a memory BAR, inside it,
    /// and clear of memory the kernel owns.
    fn arm(
        space: &mut Space,
        address: Address,
        (capability, table): &(Capability, MsiX),
        regions: &[Region],
        reserved: &Reserved,
    ) -> Result<Self, &'static str> {
        let msi = vector(address)?;
        let offset = msix::entry_offset(table, 0).ok_or("the MSI-X table is empty")?;
        let region = regions
            .iter()
            .find(|region| region.index == table.table.bar)
            .ok_or("the MSI-X table is in a BAR that was not sized")?;
        let Bar::Memory { address: base, .. } = region.bar else {
            return Err("the MSI-X table is in an I/O BAR");
        };
        if !region.contains(offset, MSIX_ENTRY_SIZE) {
            return Err("the MSI-X table entry is outside its BAR");
        }
        let phys = base
            .checked_add(offset)
            .ok_or("the MSI-X table's address overflows")?;
        if reserved.overlaps(phys, MSIX_ENTRY_SIZE) {
            return Err("the MSI-X table overlaps memory the kernel owns");
        }
        let entry = Block::map(phys, MSIX_ENTRY_SIZE)?;

        entry
            .registers
            .write32(ENTRY_VECTOR_CONTROL, VECTOR_CONTROL_MASKED);
        entry
            .registers
            .write32(ENTRY_ADDRESS_LOW, msi.address as u32);
        entry
            .registers
            .write32(ENTRY_ADDRESS_HIGH, (msi.address >> 32) as u32);
        entry.registers.write32(ENTRY_DATA, msi.data);
        entry.registers.write32(ENTRY_VECTOR_CONTROL, 0);

        let at = capability.offset + CAPABILITY_CONTROL;
        let control = space.read16(address, at);
        space.write16(
            address,
            at,
            (control | CONTROL_ENABLE) & !CONTROL_FUNCTION_MASK,
        );
        Ok(Delivery {
            capability: *capability,
            entry,
            control,
        })
    }

    /// Mask the entry and put the control word back.
    fn disarm(self, space: &mut Space, address: Address) {
        self.entry
            .registers
            .write32(ENTRY_VECTOR_CONTROL, VECTOR_CONTROL_MASKED);
        space.write16(
            address,
            self.capability.offset + CAPABILITY_CONTROL,
            self.control,
        );
    }
}

/// Why a device that refused or failed the protocol was left alone.
const fn refusal(error: TransportError) -> &'static str {
    match error {
        TransportError::ResetTimedOut => "the device did not finish resetting",
        TransportError::MissingFeatures { .. } => "the device does not offer virtio 1.x",
        TransportError::FeaturesRefused => "the device refused the features the driver wrote",
        TransportError::NoSuchQueue { .. } => "the device has no request queue",
        TransportError::QueueSize { .. } => "the device's queue cannot be sized for the check",
        TransportError::NeedsReset => "the device needs a reset",
    }
}

/// Read entropy from the virtio-rng device at `address`.
///
/// Memory decoding and bus mastering are on only for the duration, and the
/// device is reset — so it holds no address into memory this gives back —
/// before the pages are freed, MSI-X is disabled, or the command register is
/// restored. A device that will not reset keeps decoding and bus mastering
/// off and its pages are never given back.
///
/// Decoding is not turned on at all if either register block overlaps memory
/// `reserved` names. A BAR's address is whatever the register holds; on a
/// function firmware left decoding off, nothing says firmware put it there,
/// and enabling decoding over RAM or another controller's registers is
/// exactly what the aperture screening exists to prevent. Checking a BAR
/// against the host bridge's windows is the fuller answer, and the roadmap
/// says what it needs.
///
/// # Errors
///
/// Only a completion that cannot be right; see the module documentation.
pub(super) fn entropy(
    space: &mut Space,
    address: Address,
    transport: &Transport,
    regions: &[Region],
    msix: Option<&(Capability, MsiX)>,
    reserved: &Reserved,
) -> Result<Entropy, Failure> {
    if transport.common.length < COMMON_CONFIG_LEN {
        return Ok(Entropy::Skipped(
            "the common configuration block is shorter than virtio 1.x defines",
        ));
    }
    let (common, notify) = match (
        Block::locate(transport.common, regions),
        Block::locate(transport.notify, regions),
    ) {
        (Ok(common), Ok(notify)) => (common, notify),
        (Err(why), _) | (_, Err(why)) => return Ok(Entropy::Skipped(why)),
    };
    if [common, notify]
        .iter()
        .any(|&(phys, len)| reserved.overlaps(phys, len))
    {
        return Ok(Entropy::Skipped(
            "a virtio block overlaps memory the kernel owns, so decoding stays off",
        ));
    }
    let (common, notify) = match (
        Block::map(common.0, common.1),
        Block::map(notify.0, notify.1),
    ) {
        (Ok(common), Ok(notify)) => (common, notify),
        (Err(why), _) | (_, Err(why)) => return Ok(Entropy::Skipped(why)),
    };
    let (Some(rings), Some(buffer)) = (DmaPage::new(), DmaPage::new()) else {
        return Ok(Entropy::Skipped("no frame for DMA"));
    };
    // The device is given the addresses its domain gives the pages, and on a
    // translated domain reaches nothing else.
    let domain = iommu::domain_for(address);
    let Ok(pinned) = domain.pin(&[rings.frame, buffer.frame], MapFlags::DMA) else {
        return Ok(Entropy::Skipped("the DMA pages could not be pinned"));
    };
    let bus = match *pinned.addresses() {
        [rings_at, buffer_at] => (rings_at, buffer_at),
        _ => {
            pinned.leak();
            rings.leak();
            buffer.leak();
            return Err(Failure::Entropy(
                "a domain gave two pages a different number of addresses",
            ));
        }
    };

    let command = space.read16(address, COMMAND);
    space.write16(
        address,
        COMMAND,
        command | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER,
    );

    let delivery = match msix {
        Some(table) => Delivery::arm(space, address, table, regions, reserved),
        None => Err("the device has no MSI-X capability"),
    };
    let outcome = drive(
        &common,
        &notify,
        transport.notify_multiplier,
        &rings,
        &buffer,
        Granted {
            rings_at: bus.0,
            buffer_at: bus.1,
            domain: &domain,
        },
        delivery.as_ref().map(|_| ()).map_err(|why| *why),
    );

    let reset = transport::reset(&mut Common(&common), RESET_POLLS);
    if let Ok(delivery) = delivery {
        delivery.disarm(space, address);
    }
    if reset.is_err() {
        space.write16(
            address,
            COMMAND,
            command & !(COMMAND_BUS_MASTER | COMMAND_MEMORY_SPACE),
        );
        pinned.leak();
        rings.leak();
        buffer.leak();
        return match outcome {
            Err(failure) => Err(failure),
            Ok(_) => Ok(Entropy::Skipped(
                "the device did not reset, so its DMA pages are kept out of the allocator",
            )),
        };
    }
    space.write16(address, COMMAND, command);
    if domain.unpin(pinned).is_err() {
        rings.leak();
        buffer.leak();
        return Err(Failure::Entropy("the check's own domain refused its pin"));
    }
    outcome
}

/// What the check's device was granted: its two pages' device addresses, and
/// the domain that gave them.
#[derive(Clone, Copy, Debug)]
struct Granted<'a> {
    /// Where the device reaches the rings.
    rings_at: u64,
    /// Where the device reaches the buffer it writes entropy into.
    buffer_at: u64,
    /// The domain both are pinned into.
    domain: &'a iommu::Domain,
}

/// Bring the device up, make one request and wait for it: for its MSI-X
/// interrupt when `interrupt` is `Ok`, and by polling otherwise.
fn drive(
    common: &Block,
    notify: &Block,
    multiplier: u32,
    rings: &DmaPage,
    buffer: &DmaPage,
    granted: Granted<'_>,
    interrupt: Result<(), &'static str>,
) -> Result<Entropy, Failure> {
    let Granted {
        rings_at,
        buffer_at,
        domain,
    } = granted;
    let mut config = Common(common);
    // Accepted when offered: the addresses the check hands the device are
    // physical, which is what the platform's translation is until an IOMMU
    // domain is switched on, and a device that offers it refuses a driver
    // that does not accept it.
    if let Err(error) = transport::negotiate(
        &mut config,
        transport::FEATURE_ACCESS_PLATFORM,
        0,
        RESET_POLLS,
    ) {
        return Ok(Entropy::Skipped(refusal(error)));
    }
    let max = match transport::queue_max_size(&mut config, 0) {
        Ok(max) => max,
        Err(error) => return Ok(Entropy::Skipped(refusal(error))),
    };
    // The largest power of two no bigger than either.
    let size = QUEUE_SIZE.min(1 << (15 - max.leading_zeros()));
    let Ok(layout) = Layout::for_size(size) else {
        return Ok(Entropy::Skipped(
            "the device's queue cannot be sized for the check",
        ));
    };
    if layout.total_size as u64 > PAGE_SIZE {
        return Ok(Entropy::Skipped("the rings do not fit in a page"));
    }
    let base = rings_at;
    let addresses = QueueAddresses {
        descriptors: base + layout.descriptor_table as u64,
        driver: base + layout.available_ring as u64,
        device: base + layout.used_ring as u64,
    };
    // Table entry 0, which `Delivery::arm` programmed.
    let asked = if interrupt.is_ok() { 0 } else { NO_VECTOR };
    let active = match transport::activate_queue(&mut config, 0, size, addresses, asked)
        .and_then(|active| transport::driver_ok(&mut config).map(|()| active))
    {
        Ok(active) => active,
        Err(error) => return Ok(Entropy::Skipped(refusal(error))),
    };
    let polled = match interrupt {
        Err(why) => Some(why),
        Ok(()) if active.vector != asked => Some("the device kept no MSI-X vector for its queue"),
        Ok(()) => None,
    };
    let by_interrupt = polled.is_none();

    let mut queue = SplitQueue::new(layout, Rings { virt: rings.virt() });
    let head = queue.add_chain(&[Buffer::writable(buffer_at, REQUEST)])?;

    let doorbell = transport::notify_offset(active.notify_off, multiplier);
    if doorbell.checked_add(2).is_none_or(|end| end > notify.len) {
        return Ok(Entropy::Skipped(
            "the queue's doorbell is outside its block",
        ));
    }
    let before = DELIVERED.load(Ordering::SeqCst);
    let diag_cpu = || crate::smp::this_cpu().map_or(99, |cpu| cpu.logical);
    let ring_cpu = diag_cpu();
    let rung_at = timer::now_nanos();
    ring_doorbell(notify, doorbell);

    let deadline = timer::now_nanos().saturating_add(DEADLINE_NANOS);
    let mut used_seen_at = 0_u64;
    let mut last_look = rung_at;
    let mut longest_gap = 0_u64;
    let completion = loop {
        let arrived = DELIVERED.load(Ordering::SeqCst) != before;
        if used_seen_at == 0 && queue.has_used() {
            used_seen_at = timer::now_nanos();
        }
        if (!by_interrupt || arrived)
            && let Some(completion) = queue.take_used()?
        {
            break completion;
        }
        let now = timer::now_nanos();
        longest_gap = longest_gap.max(now.saturating_sub(last_look));
        last_look = now;
        if timer::now_nanos() > deadline {
            let expired_cpu = diag_cpu();
            let irqs_on = arch::interrupts_enabled();
            let held = crate::sched::preemption_held(expired_cpu);
            let at_deadline = DELIVERED.load(Ordering::SeqCst);
            let extra = timer::now_nanos().saturating_add(5_000_000_000);
            let mut late = None;
            while timer::now_nanos() < extra {
                if DELIVERED.load(Ordering::SeqCst) != before {
                    late = Some(timer::now_nanos());
                    break;
                }
                core::hint::spin_loop();
            }
            crate::println!(
                "  entdiag  skip: alloc cpu {}, ring cpu {ring_cpu}, expired cpu {expired_cpu}, \
                 irqs on {irqs_on}, preempt held {held}, delivered before {before} at deadline \
                 {at_deadline}; used seen {} ms after the doorbell; longest gap between looks {} \
                 us; late arrival {} ms after the doorbell; irq delivered {} unclaimed {}",
                DIAG_ALLOC_CPU.load(Ordering::SeqCst),
                if used_seen_at == 0 { 0 } else { (used_seen_at - rung_at) / 1_000_000 },
                longest_gap / 1_000,
                late.map_or(-1, |at| ((at - rung_at) / 1_000_000) as i64),
                irq::delivered(),
                irq::unclaimed(),
            );
            return Ok(Entropy::Skipped(if by_interrupt && queue.has_used() {
                "the device completed the request but its MSI-X interrupt never arrived"
            } else {
                "the device did not complete the request in time"
            }));
        }
        core::hint::spin_loop();
    };
    crate::println!(
        "  entdiag  ok: alloc cpu {}, ring cpu {ring_cpu}, done cpu {}, used seen {} us and taken \
         {} us after the doorbell; longest gap between looks {} us",
        DIAG_ALLOC_CPU.load(Ordering::SeqCst),
        diag_cpu(),
        if used_seen_at == 0 { 0 } else { (used_seen_at - rung_at) / 1_000 },
        (timer::now_nanos() - rung_at) / 1_000,
        longest_gap / 1_000,
    );

    if completion.head != head {
        return Err(Failure::Entropy(
            "the device completed a request nobody made",
        ));
    }
    if completion.written == 0 || completion.written > REQUEST {
        return Err(Failure::Entropy(
            "the device wrote a length it was not given",
        ));
    }
    // SAFETY: (DMA) the buffer page is the direct-map alias of a frame this check
    // owns, `written` is at most `REQUEST`, which is less than a page, and the
    // device has finished writing it.
    let bytes = unsafe {
        core::slice::from_raw_parts(buffer.virt() as *const u8, completion.written as usize)
    };
    // Eight or more zero bytes from an entropy source is a one in 2^64 event;
    // a buffer the device never wrote is not. Fewer is no evidence either way.
    if completion.written >= ZERO_EVIDENCE && bytes.iter().all(|byte| *byte == 0) {
        return Err(Failure::Entropy(
            "the device wrote nothing into the buffer it was given",
        ));
    }
    let out_of_domain = domain
        .translated()
        .then(|| probe_out_of_domain(&mut queue, notify, doorbell, domain))
        .transpose()?;
    Ok(Entropy::Read {
        bytes: completion.written,
        polled,
        out_of_domain,
    })
}

/// Where the out-of-domain probe aims: a page no translated domain of this
/// check maps, since such a domain maps only what was pinned into it and the
/// check pins only its own two frames.
const PROBE_PAGE: u64 = 0x1000;

/// How long, after the unit has recorded the fault, the probe waits to see
/// whether the device completes the request anyway. QEMU's device does so in
/// the same breath as the fault; a device that does not costs the boot this.
const COMPLETION_GRACE_NANOS: u64 = 20_000_000;

/// Ask the device to write where its domain maps nothing, and require its
/// unit to fault it: the stage 10 exit criterion's deliberate out-of-domain
/// DMA.
///
/// The unit's fault record is cleared first, because VT-d has a single record
/// and drops a second fault from the same device while it is full. Any fault
/// read here that is not the probe's is counted as stray by the domain, and
/// fails the boot in `iommu::audit_faults`. The answer is the unit's record,
/// not the device's completion: QEMU's device completes the request with the
/// length it was given even though its write was refused (see [`Faulted`]),
/// so a completion is held until the deadline and counts against the domain
/// only if no fault is recorded by then. The caller resets the device either
/// way.
///
/// # Errors
///
/// A completion saying the device wrote into the page, with no fault recorded
/// for it: DMA its domain should have stopped. A probe that could not be made,
/// or a unit that recorded nothing, is a reason in the `Ok` rather than a
/// failed boot.
fn probe_out_of_domain(
    queue: &mut SplitQueue<Rings>,
    notify: &Block,
    doorbell: u64,
    domain: &iommu::Domain,
) -> Result<Result<Faulted, &'static str>, Failure> {
    let Some(stream) = domain.stream() else {
        return Ok(Err("the domain names no stream"));
    };
    if domain.resolve(PROBE_PAGE).is_some() {
        return Ok(Err("the probe page is mapped in the domain"));
    }
    // Before the doorbell, so a fault for it is never counted as stray, even
    // one the unit records after this has stopped reading.
    domain.provoke(PROBE_PAGE);
    if queue
        .add_chain(&[Buffer::writable(PROBE_PAGE, REQUEST)])
        .is_err()
    {
        return Ok(Err("no descriptor was free for the probe"));
    }
    ring_doorbell(notify, doorbell);
    let deadline = timer::now_nanos().saturating_add(DEADLINE_NANOS);
    let mut completed = None;
    loop {
        if let Some(fault) = domain.take_fault() {
            if fault != iommu::Fault::write_to(stream, PROBE_PAGE) {
                // This fired as FX-1001 -- the probe's own page at stream 0x0
                // -- once in a few boots under KVM on a loaded host, because
                // the unit's record was read out of order and the stream came
                // from an empty record; `vtd::Unit::take_fault` explains it and
                // reads F first now. The line stays: it is the evidence that
                // said what the record held, and it is how a recurrence would
                // be read rather than deduced again.
                crate::console::println!(
                    "  pci      the unit's record holds {fault}; the probe is stream {stream:#x}, \
                     page {PROBE_PAGE:#x}, a write"
                );
                return Ok(Err("the unit recorded a fault other than the probe's"));
            }
            break;
        }
        if completed.is_none()
            && let Some(completion) = queue.take_used()?
            && completion.written > 0
        {
            completed = Some(Completed {
                written: completion.written,
                before_fault: true,
                further_faults: 0,
            });
        }
        if timer::now_nanos() > deadline {
            return if completed.is_some() {
                Err(Failure::Entropy(
                    "the device wrote into a page its domain does not map, and its unit \
                     recorded no fault",
                ))
            } else {
                Ok(Err(
                    "the unit recorded no fault for a write outside the domain",
                ))
            };
        }
        core::hint::spin_loop();
    }
    if completed.is_none() {
        let grace = timer::now_nanos().saturating_add(COMPLETION_GRACE_NANOS);
        while timer::now_nanos() <= grace {
            if let Some(completion) = queue.take_used()?
                && completion.written > 0
            {
                completed = Some(Completed {
                    written: completion.written,
                    before_fault: false,
                    further_faults: 0,
                });
                break;
            }
            core::hint::spin_loop();
        }
    }
    if let Some(completed) = completed.as_mut() {
        // Bounded, as `clear_faults` is: a device faulting without end cannot
        // keep the boot here.
        for _ in 0..256 {
            match domain.take_fault() {
                Some(fault) if fault.stream == stream && fault.page == PROBE_PAGE => {
                    completed.further_faults += 1;
                }
                Some(_) => {}
                None => break,
            }
        }
    }
    Ok(Ok(Faulted { completed }))
}
