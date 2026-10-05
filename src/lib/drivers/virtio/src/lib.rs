//! The virtio split virtqueue, as logic over an abstract shared memory.
//!
//! # Why the memory is behind a trait
//!
//! A virtqueue is three arrays in memory that two parties write to
//! concurrently: the driver owns the descriptor table and the available ring,
//! the device owns the used ring. In the kernel that memory is a DMA buffer
//! reached through a raw pointer, which makes the obvious implementation
//! untestable — a ring-index bug would only ever show up as filesystem
//! corruption on real hardware, an hour into a run.
//!
//! So the pointer arithmetic is not here. Everything in this crate is offset
//! arithmetic against a [`QueueMemory`], which the kernel implements over its
//! DMA buffer and the tests implement over a `Vec<u8>`. The logic itself
//! contains no `unsafe` at all; the single unsafe construct in the crate is the
//! [`QueueMemory`] trait declaration, whose implementor carries the obligation
//! that the memory is real.
//!
//! # Both sides
//!
//! [`SplitQueue`] is the driver: it allocates descriptors, chains them, and
//! publishes chain heads into the available ring. [`SplitQueueDevice`] is the
//! device: it consumes the available ring and writes the used ring. The device
//! side is not only for tests — Ferrix runs its drivers in userspace, so the
//! kernel is the *device* end of a virtqueue whenever it serves a userspace
//! driver, and that end needs the same range checking as anything else that
//! reads memory another protection domain wrote.
//!
//! # Trust
//!
//! Nothing read out of shared memory is trusted. Every descriptor index, ring
//! index and chain link is range-checked against the queue size before it is
//! used, and every chain walk is bounded by the queue size so that a cycle
//! terminates. A device that returns descriptor 60000 in a 128-entry queue gets
//! a [`QueueError`], not an out-of-bounds access.
//!
//! # Example
//!
//! ```
//! # extern crate std;
//! # use core::cell::RefCell;
//! # use ferrix_virtio::{Buffer, Layout, QueueMemory, SplitQueue, SplitQueueDevice};
//! # struct Mem(RefCell<std::vec::Vec<u8>>);
//! # // SAFETY: a `Vec` sized for the layout below, and the driver and device
//! # // are stepped one after the other rather than concurrently.
//! # unsafe impl QueueMemory for &Mem {
//! #     fn read_u8(&self, offset: usize) -> u8 {
//! #         self.0.borrow().get(offset).copied().unwrap_or(0)
//! #     }
//! #     fn write_u8(&mut self, offset: usize, value: u8) {
//! #         if let Some(slot) = self.0.borrow_mut().get_mut(offset) {
//! #             *slot = value;
//! #         }
//! #     }
//! #     fn read_u16(&self, offset: usize) -> u16 {
//! #         u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset + 1)])
//! #     }
//! #     fn write_u16(&mut self, offset: usize, value: u16) {
//! #         let [low, high] = value.to_le_bytes();
//! #         self.write_u8(offset, low);
//! #         self.write_u8(offset + 1, high);
//! #     }
//! #     fn barrier(&self) {}
//! # }
//! let layout = Layout::for_size(8)?;
//! let memory = Mem(RefCell::new(std::vec![0_u8; layout.total_size]));
//!
//! let mut driver = SplitQueue::new(layout, &memory);
//! let mut device = SplitQueueDevice::new(layout, &memory);
//!
//! // A request the device reads a header from and writes a payload into.
//! let head = driver.add_chain(&[Buffer::readable(0x1000, 16), Buffer::writable(0x2000, 512)])?;
//!
//! assert_eq!(device.next_chain()?, Some(head));
//! device.complete(head, 512)?;
//!
//! let completion = driver.take_used()?;
//! assert_eq!(completion.map(|done| done.written), Some(512));
//! assert_eq!(driver.free_descriptors(), 8);
//! # Ok::<(), ferrix_virtio::QueueError>(())
//! ```

#![no_std]

use core::fmt;

pub mod blk;
pub mod console;
pub mod gpu;
pub mod input;
pub mod net;
pub mod pci;
pub mod snd;

/// The size of the pages a pin returns one device address for.
///
/// Every device-specific module here lays its buffers out against this, since
/// what a driver is given is one address per page and no promise that page
/// `i + 1` follows page `i`.
pub const PAGE_SIZE: u64 = 4096;

/// A device's device-specific configuration block.
///
/// Beside the common configuration [`pci::CommonConfig`] describes, each
/// device class has a block of its own — `struct virtio_blk_config`, `struct
/// virtio_net_config` — and each module here reads its own out of one of
/// these.
///
/// The widths are there because virtio 1.2 §4.1.3.1 has a driver access each
/// field of a PCI device's configuration with its natural width, and Linux
/// does. Offsets past [`DeviceConfig::config_len`] are never passed by the
/// readers in this crate.
pub trait DeviceConfig {
    /// Bytes in the block.
    fn config_len(&self) -> u32;
    /// The byte at `offset`.
    fn config_read8(&self, offset: u32) -> u8;
    /// The little-endian `u16` at `offset`.
    fn config_read16(&self, offset: u32) -> u16;
    /// The little-endian `u32` at `offset`.
    fn config_read32(&self, offset: u32) -> u32;
}

/// A configuration block that has already been copied out, as bytes.
impl DeviceConfig for [u8] {
    fn config_len(&self) -> u32 {
        u32::try_from(self.len()).unwrap_or(u32::MAX)
    }

    fn config_read8(&self, offset: u32) -> u8 {
        byte_at(self, offset)
    }

    fn config_read16(&self, offset: u32) -> u16 {
        u16::from_le_bytes([
            byte_at(self, offset),
            byte_at(self, offset.saturating_add(1)),
        ])
    }

    fn config_read32(&self, offset: u32) -> u32 {
        u32::from_le_bytes([
            byte_at(self, offset),
            byte_at(self, offset.saturating_add(1)),
            byte_at(self, offset.saturating_add(2)),
            byte_at(self, offset.saturating_add(3)),
        ])
    }
}

/// The byte at `offset`, or zero past the end.
fn byte_at(bytes: &[u8], offset: u32) -> u8 {
    usize::try_from(offset)
        .ok()
        .and_then(|at| bytes.get(at))
        .copied()
        .unwrap_or(0)
}

/// Largest queue size a split virtqueue may have, from virtio 1.2 §2.7.
///
/// The bound is not arbitrary. Both rings index their entries with a
/// free-running `u16` that wraps at 65536, and the slot for index `i` is
/// `i % queue_size`; that mapping only stays continuous across the wrap while
/// `queue_size` divides 65536, which for a power of two means 32768 at most.
pub const MAX_QUEUE_SIZE: u16 = 32768;

/// `VIRTQ_DESC_F_NEXT`: the chain continues at [`Descriptor::next`].
pub const DESC_F_NEXT: u16 = 1;

/// `VIRTQ_DESC_F_WRITE`: the device writes this buffer rather than reads it.
pub const DESC_F_WRITE: u16 = 2;

/// `VIRTQ_DESC_F_INDIRECT`: the buffer is itself a table of descriptors.
///
/// Defined for completeness of the flags word; this crate never sets it, and a
/// caller that wants indirect descriptors builds that table itself.
pub const DESC_F_INDIRECT: u16 = 4;

/// `VIRTQ_AVAIL_F_NO_INTERRUPT`: the driver does not want to be interrupted.
///
/// It is a hint, not a barrier: virtio 1.2 §2.7.7 lets the device interrupt
/// anyway, so the driver must still cope with an interrupt it did not want.
pub const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// `VIRTQ_USED_F_NO_NOTIFY`: the device does not want to be notified.
///
/// Also a hint: a driver that notifies anyway is correct, just slower.
pub const USED_F_NO_NOTIFY: u16 = 1;

/// Driver-private marker in a descriptor's flags word: this descriptor is on
/// the free list.
///
/// virtio 1.2 §2.7.5 defines bits 0, 1 and 2 of the flags word and reserves the
/// rest. Using bit 15 is safe because it is only ever set on descriptors that
/// are *not* part of a published chain, and a conforming device only reads
/// descriptors reachable from a head it found in the available ring. It buys
/// the one thing the free list cannot otherwise answer in constant time:
/// whether a descriptor index the device just handed back is already free.
const DESC_F_FREE: u16 = 0x8000;

/// End of the free list. Not a valid descriptor index, since queue sizes stop
/// at [`MAX_QUEUE_SIZE`].
const NO_FREE: u16 = u16::MAX;

/// Size of one descriptor table entry: `addr`, `len`, `flags`, `next`.
const DESCRIPTOR_BYTES: usize = 16;

/// Size of one used ring element: `id` and `len`, both 32 bits.
const USED_ELEMENT_BYTES: usize = 8;

/// Byte offset of `len` within a descriptor.
const DESC_LEN: usize = 8;

/// Byte offset of `flags` within a descriptor.
const DESC_FLAGS: usize = 12;

/// Byte offset of `next` within a descriptor.
const DESC_NEXT: usize = 14;

/// Everything that can go wrong in a virtqueue.
///
/// The variants fall into two groups and the difference matters to a caller.
/// [`QueueError::EmptyChain`], [`QueueError::ChainTooLong`] and
/// [`QueueError::OutOfDescriptors`] are ordinary conditions a driver handles by
/// waiting or by splitting the request. The rest mean shared memory says
/// something impossible, which is a broken device or an attack, and the only
/// safe response is to stop using the queue and reset it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueueError {
    /// The queue size was zero, was not a power of two, or was above
    /// [`MAX_QUEUE_SIZE`].
    BadQueueSize,
    /// A chain was requested with no buffers in it. There is nothing to
    /// publish, and an empty chain has no head index to name.
    EmptyChain,
    /// The chain has more buffers than the queue has descriptors, so it could
    /// never be satisfied however long the caller waits.
    ChainTooLong,
    /// The free list cannot cover this chain right now. Complete something and
    /// try again.
    OutOfDescriptors,
    /// An index read out of shared memory names a descriptor the queue does not
    /// have.
    DescriptorOutOfRange,
    /// Walking a descriptor chain visited more descriptors than the queue has,
    /// which means the `next` links form a cycle.
    ChainCycle,
    /// A used entry named a descriptor that is not the head of a chain
    /// currently in flight: one already completed, one on the free list, or one
    /// in the middle of another chain.
    NotAChainHead,
    /// `used.idx` moved backwards, or forwards by more than the queue can hold.
    /// Either way the driver has lost track of which entries it has seen.
    UsedIndexJumped,
    /// `avail.idx` moved backwards, or forwards by more than the queue can
    /// hold. The device-side mirror of [`QueueError::UsedIndexJumped`].
    AvailableIndexJumped,
    /// The caller's output slice is too short for the chain being read.
    OutputTooSmall,
    /// Freeing a chain reached a descriptor already on the free list, or would
    /// have made more descriptors free than the queue has. The device changed
    /// the chain's links after it was validated.
    FreeListCorrupt,
}

/// Where the three areas of a split virtqueue sit within one block of memory.
///
/// The offsets follow virtio 1.2 §2.7: the descriptor table is 16-byte aligned,
/// the available ring 2-byte aligned and the used ring 4-byte aligned, and the
/// used ring is padded away from the end of the available ring to reach its
/// alignment. The descriptor table starts at offset zero, so the alignment of
/// the block as a whole — which the [`QueueMemory`] implementor promises — is
/// what makes every area's alignment hold.
///
/// Both rings are sized including their event-suppression field (`used_event`
/// and `avail_event`). Those are only read when `VIRTIO_F_EVENT_IDX` is
/// negotiated, but the space is reserved either way, so the layout does not
/// move under a driver that negotiates the feature later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// Number of descriptors, and the number of entries in each ring: a power
    /// of two, at least one, at most [`MAX_QUEUE_SIZE`].
    pub queue_size: u16,
    /// Byte offset of the descriptor table. Always zero; named so that callers
    /// do not have to know that.
    pub descriptor_table: usize,
    /// Byte offset of the available ring: `flags`, `idx`, `ring`, `used_event`.
    pub available_ring: usize,
    /// Byte offset of the used ring: `flags`, `idx`, `ring`, `avail_event`.
    pub used_ring: usize,
    /// Total bytes the three areas occupy, including the padding between the
    /// available and used rings.
    pub total_size: usize,
}

impl Layout {
    /// Compute the layout for a queue of `queue_size` descriptors.
    ///
    /// # Errors
    ///
    /// [`QueueError::BadQueueSize`] if `queue_size` is zero, is not a power of
    /// two, or exceeds [`MAX_QUEUE_SIZE`].
    pub const fn for_size(queue_size: u16) -> Result<Self, QueueError> {
        if queue_size == 0 || queue_size > MAX_QUEUE_SIZE || !queue_size.is_power_of_two() {
            return Err(QueueError::BadQueueSize);
        }

        let size = queue_size as usize;
        let descriptor_table = 0;
        let available_ring = descriptor_table + size * DESCRIPTOR_BYTES;
        // `flags`, `idx` and `used_event` are two bytes each; the ring itself
        // is one `u16` per descriptor.
        let available_end = available_ring + 6 + size * 2;
        let used_ring = align_up(available_end, 4);
        // `flags`, `idx` and `avail_event` are two bytes each; the ring itself
        // is an eight-byte element per descriptor.
        let total_size = used_ring + 6 + size * USED_ELEMENT_BYTES;

        Ok(Self {
            queue_size,
            descriptor_table,
            available_ring,
            used_ring,
            total_size,
        })
    }

    /// Byte offset of descriptor `index`, which the caller must already have
    /// range-checked against [`Layout::queue_size`].
    const fn descriptor(&self, index: u16) -> usize {
        self.descriptor_table + index as usize * DESCRIPTOR_BYTES
    }

    /// Byte offset of the available ring's `flags`.
    const fn available_flags(&self) -> usize {
        self.available_ring
    }

    /// Byte offset of the available ring's `idx`.
    const fn available_idx(&self) -> usize {
        self.available_ring + 2
    }

    /// Byte offset of available ring slot `slot`, which the caller must already
    /// have reduced modulo [`Layout::queue_size`].
    const fn available_entry(&self, slot: u16) -> usize {
        self.available_ring + 4 + slot as usize * 2
    }

    /// Byte offset of `used_event`, the driver's event-index suppression field.
    const fn used_event(&self) -> usize {
        self.available_ring + 4 + self.queue_size as usize * 2
    }

    /// Byte offset of the used ring's `flags`.
    const fn used_flags(&self) -> usize {
        self.used_ring
    }

    /// Byte offset of the used ring's `idx`.
    const fn used_idx(&self) -> usize {
        self.used_ring + 2
    }

    /// Byte offset of used ring slot `slot`, which the caller must already have
    /// reduced modulo [`Layout::queue_size`].
    const fn used_entry(&self, slot: u16) -> usize {
        self.used_ring + 4 + slot as usize * USED_ELEMENT_BYTES
    }

    /// Byte offset of `avail_event`, the device's event-index suppression
    /// field.
    const fn avail_event(&self) -> usize {
        self.used_ring + 4 + self.queue_size as usize * USED_ELEMENT_BYTES
    }
}

/// Round `value` up to a multiple of `align`, which must be a power of two.
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// One buffer to hand to the device.
///
/// The address is guest-physical because that is what the device's DMA engine
/// dereferences. Translating from whatever the caller holds is the caller's
/// job, and getting it wrong is this project's characteristic bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Buffer {
    /// Guest-physical address of the buffer.
    pub address: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Whether the device writes this buffer (`true`) or reads it (`false`).
    pub device_writable: bool,
}

impl Buffer {
    /// A buffer the device reads: a request header, or data being written out.
    #[must_use]
    pub const fn readable(address: u64, len: u32) -> Self {
        Self {
            address,
            len,
            device_writable: false,
        }
    }

    /// A buffer the device writes: a status byte, or data being read in.
    #[must_use]
    pub const fn writable(address: u64, len: u32) -> Self {
        Self {
            address,
            len,
            device_writable: true,
        }
    }
}

/// One entry of the descriptor table, as it sits in memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Descriptor {
    /// Guest-physical address of the buffer.
    pub address: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Flags: [`DESC_F_NEXT`], [`DESC_F_WRITE`], [`DESC_F_INDIRECT`].
    pub flags: u16,
    /// Index of the next descriptor, meaningful only when [`DESC_F_NEXT`] is
    /// set in [`Descriptor::flags`].
    pub next: u16,
}

impl Descriptor {
    /// Whether the chain continues at [`Descriptor::next`].
    #[must_use]
    pub const fn has_next(&self) -> bool {
        self.flags & DESC_F_NEXT != 0
    }

    /// Whether the device writes this buffer rather than reading it.
    #[must_use]
    pub const fn is_device_writable(&self) -> bool {
        self.flags & DESC_F_WRITE != 0
    }
}

/// A finished request, as reported by the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Completion {
    /// The head descriptor index the driver published for this chain.
    pub head: u16,
    /// Bytes the device says it wrote into the chain's writable buffers.
    ///
    /// The device chooses this number, so a driver must treat it as untrusted:
    /// clamp it against the buffer lengths it actually supplied before using it
    /// to bound a copy.
    pub written: u32,
}

/// The shared memory a virtqueue lives in.
///
/// [`QueueMemory::read_u8`], [`QueueMemory::write_u8`],
/// [`QueueMemory::read_u16`], [`QueueMemory::write_u16`] and
/// [`QueueMemory::barrier`] have to be implemented; the wider accessors are
/// provided in terms of them, little-endian because virtio 1.x defines every
/// field in the rings as little-endian on the wire whatever the host is.
///
/// **The `u16` accessors are not provided, and each must be one access.** The
/// rings' indices and event fields -- `avail.idx`, `used.idx`, `used_event`,
/// `avail_event` and the two flags words -- are `u16`s the other side reads
/// and writes while this one does. Written as two bytes, `avail.idx` stepping
/// from `0x01FF` to `0x0200` is `0x0100` for an instant, and a device that
/// reads it then has been told the ring ran backwards; read as two bytes,
/// `used.idx` can come back with one byte from before the device's store and
/// one from after. These used to be provided in terms of the byte accessors,
/// and every driver in the tree inherited that. The wider accessors are left
/// provided because nothing reads a `u32` or `u64` while the other side
/// writes it: a descriptor is written before it is published, and a used
/// entry before `used.idx` says so.
///
/// An implementation that gives only the byte accessors does not compile:
///
/// ```compile_fail,E0046
/// struct Bytes;
/// // SAFETY: never used; this only has to fail to compile.
/// unsafe impl ferrix_virtio::QueueMemory for Bytes {
///     fn read_u8(&self, _offset: usize) -> u8 {
///         0
///     }
///     fn write_u8(&mut self, _offset: usize, _value: u8) {}
///     fn barrier(&self) {}
/// }
/// ```
///
/// # Safety
///
/// Implementors must provide access to real, exclusively-owned memory of at
/// least [`Layout::total_size`] bytes for the layout the queue is built with,
/// starting at an address aligned to at least 16 bytes so that the descriptor
/// table, available ring and used ring each land on the alignment virtio 1.2
/// §2.7 requires. Every offset this crate passes is within that range.
///
/// "Exclusively owned" is meant with respect to the rest of this address space.
/// The other side of the ring writes to this memory concurrently by design,
/// which is why every field read back out of it here is validated rather than
/// trusted.
///
/// [`QueueMemory::read_u16`] and [`QueueMemory::write_u16`] must each be a
/// single access of the whole naturally aligned `u16`, never two of a byte,
/// wherever the other side runs concurrently. A 16-bit access is single-copy
/// atomic only when it is aligned, on Arm as on x86, and that holds by
/// construction here: the memory starts 16-byte aligned, as above, and
/// [`Layout`] puts every `u16` the crate passes at an even offset (the
/// `u16`s of the available ring after a table of 16-byte descriptors, the used
/// ring rounded up to 4; `every_shared_field_is_even` checks every size).
/// Memory only one side touches at a time, as a test's is, may compose them
/// from bytes.
///
/// [`QueueMemory::barrier`] must be a real ordering fence for whatever couples
/// the two sides — a `fence(SeqCst)`, a `dsb sy`, whatever the platform needs
/// so that accesses issued before it are visible to the other side before
/// accesses issued after it. An empty `barrier` leaves the ring subtly wrong
/// under concurrency, in a way no single-threaded test can detect.
pub unsafe trait QueueMemory {
    /// Read one byte at `offset`.
    fn read_u8(&self, offset: usize) -> u8;

    /// Write one byte at `offset`.
    fn write_u8(&mut self, offset: usize, value: u8);

    /// Order every access issued before this call against every access issued
    /// after it.
    fn barrier(&self);

    /// Read the little-endian `u16` at `offset`, in one access.
    fn read_u16(&self, offset: usize) -> u16;

    /// Read a little-endian `u32` at `offset`.
    fn read_u32(&self, offset: usize) -> u32 {
        u32::from(self.read_u16(offset)) | (u32::from(self.read_u16(offset + 2)) << 16)
    }

    /// Read a little-endian `u64` at `offset`.
    fn read_u64(&self, offset: usize) -> u64 {
        u64::from(self.read_u32(offset)) | (u64::from(self.read_u32(offset + 4)) << 32)
    }

    /// Write the little-endian `u16` at `offset`, in one access.
    fn write_u16(&mut self, offset: usize, value: u16);

    /// Write a little-endian `u32` at `offset`.
    fn write_u32(&mut self, offset: usize, value: u32) {
        self.write_u16(offset, value as u16);
        self.write_u16(offset + 2, (value >> 16) as u16);
    }

    /// Write a little-endian `u64` at `offset`.
    fn write_u64(&mut self, offset: usize, value: u64) {
        self.write_u32(offset, value as u32);
        self.write_u32(offset + 4, (value >> 32) as u32);
    }
}

/// The driver side of a split virtqueue.
///
/// Owns the descriptor table and the available ring, and reads the used ring.
///
/// Free descriptors are kept as a singly linked stack threaded through the
/// descriptors' own `next` fields, which is what that field is for while a
/// descriptor is in no chain. The list head and the count live here rather than
/// in shared memory, so that the ordinary path never depends on a value the
/// device could have scribbled on.
pub struct SplitQueue<M> {
    /// Where the three areas are.
    layout: Layout,
    /// The shared memory itself.
    memory: M,
    /// First descriptor on the free list, or [`NO_FREE`] when it is empty.
    free_head: u16,
    /// How many descriptors the free list holds.
    free_count: u16,
    /// Shadow of `avail.idx`: chains ever published, wrapping at 65536.
    avail_idx: u16,
    /// Used entries ever consumed, wrapping at 65536. However far the device's
    /// `used.idx` runs ahead of this is exactly the work waiting for us.
    last_used_idx: u16,
}

impl<M> fmt::Debug for SplitQueue<M> {
    /// Deliberately does not print the memory: it is a DMA window in the
    /// kernel, and formatting it would mean a great many reads with side
    /// effects.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SplitQueue")
            .field("queue_size", &self.layout.queue_size)
            .field("free_count", &self.free_count)
            .field("avail_idx", &self.avail_idx)
            .field("last_used_idx", &self.last_used_idx)
            .finish_non_exhaustive()
    }
}

impl<M: QueueMemory> SplitQueue<M> {
    /// Take over a freshly allocated, zeroed queue and build its free list.
    ///
    /// This resets both rings' indices, so it belongs before the device is told
    /// about the queue rather than on a queue already in flight. Every
    /// descriptor starts free, so [`SplitQueue::free_descriptors`] returns
    /// `layout.queue_size`.
    pub fn new(layout: Layout, memory: M) -> Self {
        let mut queue = Self {
            layout,
            memory,
            free_head: NO_FREE,
            free_count: 0,
            avail_idx: 0,
            last_used_idx: 0,
        };

        // Built back to front so the list comes out in ascending order, which
        // makes a queue dump readable and lets a test predict which descriptor
        // a chain will get.
        let mut index = layout.queue_size;
        while index > 0 {
            index -= 1;
            // A fresh queue has nothing free, so this cannot refuse.
            if queue.push_free(index).is_err() {
                break;
            }
        }

        queue.memory.write_u16(layout.available_flags(), 0);
        queue.memory.write_u16(layout.available_idx(), 0);
        queue.memory.write_u16(layout.used_event(), 0);
        queue.memory.write_u16(layout.used_flags(), 0);
        queue.memory.write_u16(layout.used_idx(), 0);
        queue.memory.write_u16(layout.avail_event(), 0);

        queue
    }

    /// Give the memory back, for a queue built on it again after the device
    /// has been reset: [`SplitQueue::new`] writes every index afresh.
    #[must_use]
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// The layout this queue was built with.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The shared memory, for a caller that needs to inspect it.
    #[must_use]
    pub const fn memory(&self) -> &M {
        &self.memory
    }

    /// How many descriptors are on the free list.
    ///
    /// A chain of `n` buffers needs `n` of them. Watching this fail to return
    /// to the queue size across a quiet period is how a leaked chain tail is
    /// caught before it exhausts the queue.
    #[must_use]
    pub const fn free_descriptors(&self) -> u16 {
        self.free_count
    }

    /// Chains published so far, wrapping at 65536. This is `avail.idx`.
    #[must_use]
    pub const fn available_index(&self) -> u16 {
        self.avail_idx
    }

    /// Publish a chain of buffers and return its head descriptor index.
    ///
    /// The head is what the device names when it completes the request, so a
    /// driver that keys its in-flight requests on it can find the request
    /// again.
    ///
    /// # Ordering
    ///
    /// The descriptors and the available ring entry are written, then
    /// [`QueueMemory::barrier`] runs, then `avail.idx` is updated. The device
    /// may look at the ring the instant `avail.idx` changes, so without that
    /// barrier it can read a descriptor the driver has not finished writing.
    ///
    /// # Errors
    ///
    /// [`QueueError::EmptyChain`] for no buffers, [`QueueError::ChainTooLong`]
    /// for more buffers than the queue has descriptors, and
    /// [`QueueError::OutOfDescriptors`] when the free list is too short — all
    /// checked before anything is written, so a rejected chain leaves the queue
    /// exactly as it was.
    pub fn add_chain(&mut self, buffers: &[Buffer]) -> Result<u16, QueueError> {
        let Some(first) = buffers.first() else {
            return Err(QueueError::EmptyChain);
        };
        if buffers.len() > self.layout.queue_size as usize {
            return Err(QueueError::ChainTooLong);
        }
        if buffers.len() > self.free_count as usize {
            return Err(QueueError::OutOfDescriptors);
        }

        let head = self.pop_free()?;
        self.write_descriptor(head, first, head);

        let mut previous = head;
        for buffer in buffers.iter().skip(1) {
            let index = match self.pop_free() {
                Ok(index) => index,
                // Only reachable if the free list in memory disagrees with the
                // count checked above, which means corruption. Give back the
                // descriptors already taken rather than leaking them.
                Err(error) => {
                    let _ = self.free_chain(head);
                    return Err(error);
                }
            };
            self.link(previous, index);
            self.write_descriptor(index, buffer, head);
            previous = index;
        }

        self.publish(head);
        Ok(head)
    }

    /// Whether the device has completed anything the driver has not taken.
    #[must_use]
    pub fn has_used(&self) -> bool {
        self.memory.read_u16(self.layout.used_idx()) != self.last_used_idx
    }

    /// Take the next completion, freeing the whole chain behind it.
    ///
    /// `Ok(None)` means the device has completed nothing new. An error means
    /// the used ring says something impossible; the entry is *not* consumed and
    /// no descriptors are freed, so the caller can reset the queue without
    /// having already acted on a bad entry.
    ///
    /// Freeing the whole chain rather than only the head is the point of this
    /// method. A driver that frees only the head leaks every interior
    /// descriptor, which stays invisible until the queue stops accepting chains
    /// some hours into a run.
    ///
    /// # Ordering
    ///
    /// `used.idx` is read, then [`QueueMemory::barrier`] runs, then the entry
    /// it points at is read. The other order can read the ring slot before the
    /// device has finished writing it and see the previous lap's completion.
    ///
    /// # Errors
    ///
    /// [`QueueError::UsedIndexJumped`] if `used.idx` moved impossibly,
    /// [`QueueError::DescriptorOutOfRange`] if the entry names a descriptor
    /// that does not exist, [`QueueError::NotAChainHead`] if it names one that
    /// is not the head of a chain in flight, and [`QueueError::ChainCycle`] if
    /// the chain's links form a loop.
    pub fn take_used(&mut self) -> Result<Option<Completion>, QueueError> {
        let used_idx = self.memory.read_u16(self.layout.used_idx());
        if used_idx == self.last_used_idx {
            return Ok(None);
        }
        // An unsigned wrapping distance: a backwards jump shows up as an
        // enormous forwards one, and either way more than a queue's worth of
        // outstanding completions cannot exist.
        if used_idx.wrapping_sub(self.last_used_idx) > self.layout.queue_size {
            return Err(QueueError::UsedIndexJumped);
        }

        self.memory.barrier();

        let slot = self.last_used_idx % self.layout.queue_size;
        let entry = self.layout.used_entry(slot);
        let id = self.memory.read_u32(entry);
        let written = self.memory.read_u32(entry + 4);

        if id >= u32::from(self.layout.queue_size) {
            return Err(QueueError::DescriptorOutOfRange);
        }
        // Below `queue_size`, which is at most 32768, so this cannot truncate.
        let head = id as u16;

        self.validate_head(head)?;
        let _ = self.free_chain(head)?;

        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Ok(Some(Completion { head, written }))
    }

    /// Read descriptor `index` back out of shared memory.
    ///
    /// For tests, and for a driver dumping a queue that has gone wrong. A
    /// descriptor on the free list carries a driver-private marker bit in its
    /// flags, so do not compare the flags word of an arbitrary descriptor with
    /// a constant for equality.
    ///
    /// # Errors
    ///
    /// [`QueueError::DescriptorOutOfRange`] if `index` is not a descriptor of
    /// this queue.
    pub fn descriptor(&self, index: u16) -> Result<Descriptor, QueueError> {
        if index >= self.layout.queue_size {
            return Err(QueueError::DescriptorOutOfRange);
        }
        Ok(self.read_descriptor(index))
    }

    /// Ask the device not to interrupt on completion, or withdraw the request.
    ///
    /// This is the flag a driver clears before it sleeps and sets while it is
    /// polling. It is advisory (virtio 1.2 §2.7.7): the device may interrupt
    /// regardless, so the handler must tolerate having nothing to do.
    pub fn set_interrupts_suppressed(&mut self, suppressed: bool) {
        let flags = if suppressed { AVAIL_F_NO_INTERRUPT } else { 0 };
        self.memory.write_u16(self.layout.available_flags(), flags);
    }

    /// Whether the driver has asked not to be interrupted.
    #[must_use]
    pub fn interrupts_suppressed(&self) -> bool {
        self.memory.read_u16(self.layout.available_flags()) & AVAIL_F_NO_INTERRUPT != 0
    }

    /// Whether the device wants to be notified after a chain is published.
    ///
    /// Reads the device's `used.flags`. Skipping the notification is an
    /// optimisation the device offered; notifying anyway is always correct.
    ///
    /// # Ordering
    ///
    /// [`QueueMemory::barrier`] runs first, between the `avail.idx` store
    /// that published the chain and this load of `used.flags`. The device
    /// does the mirror image -- clears `USED_F_NO_NOTIFY`, fences, and reads
    /// `avail.idx` once more before it sleeps -- and each side's fence is
    /// what keeps both from missing the other. Without this one, x86 lets
    /// the load pass the store still in the store buffer: the driver reads
    /// the flag the device is just clearing and skips the notification,
    /// the device reads the index from before the publish and sleeps, and
    /// the chain sits in the available ring until something else notifies.
    /// Linux's `virtqueue_kick_prepare` fences here for the same reason.
    #[must_use]
    pub fn device_wants_notification(&self) -> bool {
        self.memory.barrier();
        self.memory.read_u16(self.layout.used_flags()) & USED_F_NO_NOTIFY == 0
    }

    /// Set `used_event`: with `VIRTIO_F_EVENT_IDX` negotiated, the device
    /// interrupts only once `used.idx` reaches this value.
    pub fn set_used_event(&mut self, value: u16) {
        self.memory.write_u16(self.layout.used_event(), value);
    }

    /// Read `avail_event`, the index at which the device wants notifying.
    #[must_use]
    pub fn avail_event(&self) -> u16 {
        self.memory.read_u16(self.layout.avail_event())
    }

    /// Put `index` on the free list, wiping the buffer it described.
    ///
    /// The wipe is not hygiene: a stale address left in a descriptor the device
    /// is not supposed to be looking at is one bug away from being DMA'd to.
    ///
    /// # Errors
    ///
    /// [`QueueError::FreeListCorrupt`] if every descriptor is already free,
    /// which a correct free list cannot reach and a hostile device can.
    fn push_free(&mut self, index: u16) -> Result<(), QueueError> {
        if self.free_count >= self.layout.queue_size {
            return Err(QueueError::FreeListCorrupt);
        }
        let offset = self.layout.descriptor(index);
        self.memory.write_u64(offset, 0);
        self.memory.write_u32(offset + DESC_LEN, 0);
        self.memory.write_u16(offset + DESC_FLAGS, DESC_F_FREE);
        self.memory.write_u16(offset + DESC_NEXT, self.free_head);
        self.free_head = index;
        self.free_count += 1;
        Ok(())
    }

    /// Take a descriptor off the free list.
    fn pop_free(&mut self) -> Result<u16, QueueError> {
        if self.free_count == 0 {
            return Err(QueueError::OutOfDescriptors);
        }
        let head = self.free_head;
        if head >= self.layout.queue_size {
            return Err(QueueError::DescriptorOutOfRange);
        }

        let next = self
            .memory
            .read_u16(self.layout.descriptor(head) + DESC_NEXT);
        if self.free_count == 1 {
            self.free_head = NO_FREE;
        } else if next < self.layout.queue_size {
            self.free_head = next;
        } else {
            // The count says the free list continues and the link says it does
            // not. Stop rather than hand the same descriptor out twice.
            return Err(QueueError::DescriptorOutOfRange);
        }
        self.free_count -= 1;
        Ok(head)
    }

    /// Write one descriptor for `buffer`.
    ///
    /// `back` goes in the `next` field, where the device cannot see it because
    /// [`DESC_F_NEXT`] is clear (virtio 1.2 §2.7.5: `next` is only meaningful
    /// when the flag is set). It holds the index of the chain's own head, which
    /// is what lets [`SplitQueue::validate_head`] tell a head from an interior
    /// descriptor without a side table. [`SplitQueue::link`] overwrites it if
    /// the chain turns out to continue.
    fn write_descriptor(&mut self, index: u16, buffer: &Buffer, back: u16) {
        let flags = if buffer.device_writable {
            DESC_F_WRITE
        } else {
            0
        };
        let offset = self.layout.descriptor(index);
        self.memory.write_u64(offset, buffer.address);
        self.memory.write_u32(offset + DESC_LEN, buffer.len);
        self.memory.write_u16(offset + DESC_FLAGS, flags);
        self.memory.write_u16(offset + DESC_NEXT, back);
    }

    /// Continue the chain from `previous` into `next`.
    fn link(&mut self, previous: u16, next: u16) {
        let offset = self.layout.descriptor(previous);
        let flags = self.memory.read_u16(offset + DESC_FLAGS);
        self.memory
            .write_u16(offset + DESC_FLAGS, flags | DESC_F_NEXT);
        self.memory.write_u16(offset + DESC_NEXT, next);
    }

    /// Read descriptor `index`, which the caller has range-checked.
    fn read_descriptor(&self, index: u16) -> Descriptor {
        let offset = self.layout.descriptor(index);
        Descriptor {
            address: self.memory.read_u64(offset),
            len: self.memory.read_u32(offset + DESC_LEN),
            flags: self.memory.read_u16(offset + DESC_FLAGS),
            next: self.memory.read_u16(offset + DESC_NEXT),
        }
    }

    /// Put the chain head into the available ring and make it visible.
    ///
    /// `avail.idx` counts chains ever published and wraps at 65536; only the
    /// ring slot is reduced modulo the queue size. Masking the index itself is
    /// the classic virtqueue bug, and on a 128-entry queue it stays invisible
    /// for the first 65536 requests.
    fn publish(&mut self, head: u16) {
        let slot = self.avail_idx % self.layout.queue_size;
        self.memory
            .write_u16(self.layout.available_entry(slot), head);

        // Everything above has to be visible to the device before the index
        // change that invites it to look. This is the barrier the whole
        // protocol rests on.
        self.memory.barrier();

        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.memory
            .write_u16(self.layout.available_idx(), self.avail_idx);
    }

    /// Check that `head` really is the head of a chain currently in flight.
    ///
    /// A device may put any `u16` in the used ring. Three things are checked:
    /// that the descriptor is not on the free list, that the walk terminates
    /// within `queue_size` steps, and that the tail's back-pointer names `head`
    /// itself. The last is what rejects an interior descriptor of a live chain,
    /// whose tail points back at the real head instead.
    fn validate_head(&self, head: u16) -> Result<(), QueueError> {
        let size = self.layout.queue_size;
        if head >= size {
            return Err(QueueError::DescriptorOutOfRange);
        }

        let mut index = head;
        let mut steps: u16 = 0;
        loop {
            let offset = self.layout.descriptor(index);
            let flags = self.memory.read_u16(offset + DESC_FLAGS);
            let next = self.memory.read_u16(offset + DESC_NEXT);
            if flags & DESC_F_FREE != 0 {
                return Err(QueueError::NotAChainHead);
            }
            steps += 1;
            if flags & DESC_F_NEXT == 0 {
                if next == head {
                    return Ok(());
                }
                return Err(QueueError::NotAChainHead);
            }
            if steps >= size {
                return Err(QueueError::ChainCycle);
            }
            if next >= size {
                return Err(QueueError::DescriptorOutOfRange);
            }
            index = next;
        }
    }

    /// Free every descriptor of the chain at `head` and report how many there
    /// were.
    ///
    /// The walk is bounded by the queue size, so a cycle ends as an error
    /// rather than as a hang.
    fn free_chain(&mut self, head: u16) -> Result<u16, QueueError> {
        let size = self.layout.queue_size;
        let mut index = head;
        let mut freed: u16 = 0;
        loop {
            if index >= size {
                return Err(QueueError::DescriptorOutOfRange);
            }
            let offset = self.layout.descriptor(index);
            // Both are read before `push_free` overwrites them.
            let flags = self.memory.read_u16(offset + DESC_FLAGS);
            let next = self.memory.read_u16(offset + DESC_NEXT);
            // The chain was validated, and then read again here from memory
            // the device can write: a link it moved in between can reach a
            // descriptor already free, and freeing that twice would put one
            // descriptor on the list twice and hand it to two chains.
            if flags & DESC_F_FREE != 0 {
                return Err(QueueError::FreeListCorrupt);
            }
            self.push_free(index)?;
            freed += 1;
            if flags & DESC_F_NEXT == 0 {
                return Ok(freed);
            }
            if freed >= size {
                return Err(QueueError::ChainCycle);
            }
            index = next;
        }
    }
}

/// The device side of a split virtqueue.
///
/// Consumes the available ring and writes the used ring. In Ferrix this is the
/// kernel end of a ring shared with a userspace driver, so it treats the
/// available ring exactly as sceptically as [`SplitQueue`] treats the used
/// ring: every index range-checked, every chain walk bounded.
pub struct SplitQueueDevice<M> {
    /// Where the three areas are.
    layout: Layout,
    /// The shared memory itself.
    memory: M,
    /// Available entries consumed so far, wrapping at 65536.
    last_avail_idx: u16,
    /// Shadow of `used.idx`: completions published, wrapping at 65536.
    used_idx: u16,
}

impl<M> fmt::Debug for SplitQueueDevice<M> {
    /// Does not print the memory, for the reason [`SplitQueue`] does not.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SplitQueueDevice")
            .field("queue_size", &self.layout.queue_size)
            .field("last_avail_idx", &self.last_avail_idx)
            .field("used_idx", &self.used_idx)
            .finish_non_exhaustive()
    }
}

impl<M: QueueMemory> SplitQueueDevice<M> {
    /// Attach to a queue that has just been reset.
    ///
    /// Both indices start at zero because that is where the driver's start;
    /// this is not a way to attach to a queue that is already running.
    pub const fn new(layout: Layout, memory: M) -> Self {
        Self {
            layout,
            memory,
            last_avail_idx: 0,
            used_idx: 0,
        }
    }

    /// The layout this queue was built with.
    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The shared memory, for a caller that needs to inspect it.
    #[must_use]
    pub const fn memory(&self) -> &M {
        &self.memory
    }

    /// Completions published so far, wrapping at 65536. This is `used.idx`.
    #[must_use]
    pub const fn used_index(&self) -> u16 {
        self.used_idx
    }

    /// Whether the driver has published a chain the device has not taken.
    #[must_use]
    pub fn has_available(&self) -> bool {
        self.memory.read_u16(self.layout.available_idx()) != self.last_avail_idx
    }

    /// Take the head index of the next chain the driver published.
    ///
    /// # Ordering
    ///
    /// `avail.idx` is read, then [`QueueMemory::barrier`] runs, then the ring
    /// entry is read — the mirror of what the driver does on the used ring, and
    /// necessary for the same reason.
    ///
    /// # Errors
    ///
    /// [`QueueError::AvailableIndexJumped`] if `avail.idx` moved impossibly,
    /// and [`QueueError::DescriptorOutOfRange`] if the entry names a descriptor
    /// this queue does not have.
    pub fn next_chain(&mut self) -> Result<Option<u16>, QueueError> {
        let avail_idx = self.memory.read_u16(self.layout.available_idx());
        if avail_idx == self.last_avail_idx {
            return Ok(None);
        }
        if avail_idx.wrapping_sub(self.last_avail_idx) > self.layout.queue_size {
            return Err(QueueError::AvailableIndexJumped);
        }

        self.memory.barrier();

        let slot = self.last_avail_idx % self.layout.queue_size;
        let head = self.memory.read_u16(self.layout.available_entry(slot));
        if head >= self.layout.queue_size {
            return Err(QueueError::DescriptorOutOfRange);
        }

        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        Ok(Some(head))
    }

    /// Read the chain starting at `head` into `out`, returning its length.
    ///
    /// # Errors
    ///
    /// [`QueueError::DescriptorOutOfRange`] for an index outside the queue,
    /// [`QueueError::ChainCycle`] if the links loop, and
    /// [`QueueError::OutputTooSmall`] if `out` cannot hold the whole chain.
    pub fn read_chain(&self, head: u16, out: &mut [Descriptor]) -> Result<usize, QueueError> {
        let size = self.layout.queue_size;
        let mut index = head;
        let mut count: usize = 0;
        loop {
            if index >= size {
                return Err(QueueError::DescriptorOutOfRange);
            }
            let descriptor = self.read_descriptor(index);
            let Some(slot) = out.get_mut(count) else {
                return Err(QueueError::OutputTooSmall);
            };
            *slot = descriptor;
            count += 1;
            if !descriptor.has_next() {
                return Ok(count);
            }
            if count >= size as usize {
                return Err(QueueError::ChainCycle);
            }
            index = descriptor.next;
        }
    }

    /// Report a chain finished, having written `written` bytes into it.
    ///
    /// # Ordering
    ///
    /// The used entry is written, then [`QueueMemory::barrier`] runs, then
    /// `used.idx` is updated, so the driver cannot see an index pointing at a
    /// half-written entry.
    ///
    /// # Errors
    ///
    /// [`QueueError::DescriptorOutOfRange`] if `head` is not a descriptor of
    /// this queue.
    pub fn complete(&mut self, head: u16, written: u32) -> Result<(), QueueError> {
        if head >= self.layout.queue_size {
            return Err(QueueError::DescriptorOutOfRange);
        }

        let slot = self.used_idx % self.layout.queue_size;
        let entry = self.layout.used_entry(slot);
        self.memory.write_u32(entry, u32::from(head));
        self.memory.write_u32(entry + 4, written);

        self.memory.barrier();

        self.used_idx = self.used_idx.wrapping_add(1);
        self.memory.write_u16(self.layout.used_idx(), self.used_idx);
        Ok(())
    }

    /// Tell the driver it need not notify on publish, or withdraw that.
    pub fn set_notifications_suppressed(&mut self, suppressed: bool) {
        let flags = if suppressed { USED_F_NO_NOTIFY } else { 0 };
        self.memory.write_u16(self.layout.used_flags(), flags);
    }

    /// Whether the device has asked not to be notified.
    #[must_use]
    pub fn notifications_suppressed(&self) -> bool {
        self.memory.read_u16(self.layout.used_flags()) & USED_F_NO_NOTIFY != 0
    }

    /// Whether the driver wants an interrupt when a chain completes.
    ///
    /// [`QueueMemory::barrier`] runs first, between the `used.idx` store of
    /// [`SplitQueueDevice::complete`] and this load, for the reason
    /// [`SplitQueue::device_wants_notification`] gives from the other side.
    #[must_use]
    pub fn driver_wants_interrupt(&self) -> bool {
        self.memory.barrier();
        self.memory.read_u16(self.layout.available_flags()) & AVAIL_F_NO_INTERRUPT == 0
    }

    /// Set `avail_event`: with `VIRTIO_F_EVENT_IDX` negotiated, the driver
    /// notifies only once `avail.idx` reaches this value.
    pub fn set_avail_event(&mut self, value: u16) {
        self.memory.write_u16(self.layout.avail_event(), value);
    }

    /// Read `used_event`, the index at which the driver wants interrupting.
    #[must_use]
    pub fn used_event(&self) -> u16 {
        self.memory.read_u16(self.layout.used_event())
    }

    /// Read descriptor `index`, which the caller has range-checked.
    fn read_descriptor(&self, index: u16) -> Descriptor {
        let offset = self.layout.descriptor(index);
        Descriptor {
            address: self.memory.read_u64(offset),
            len: self.memory.read_u32(offset + DESC_LEN),
            flags: self.memory.read_u16(offset + DESC_FLAGS),
            next: self.memory.read_u16(offset + DESC_NEXT),
        }
    }
}

#[cfg(test)]
mod tests;
