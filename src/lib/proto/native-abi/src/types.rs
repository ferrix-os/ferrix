//! The structures native calls read and write through pointers.
//!
//! Every one is laid out so that it has no padding and the same size and
//! offsets on all three targets. A `u64` is 8-aligned under both the x86-64
//! System V ABI and AAPCS, so an ARMv7-A program and the kernel agree on
//! these without a 32-bit variant — which is the property Linux's `stat`
//! does not have, and why `fstat64` exists.

/// The most bytes one channel message may carry.
///
/// Sixty-four kibibytes: large enough for any control message a driver
/// sends, and small enough that a message is copied through the kernel
/// rather than mapped. Bulk data belongs in a shared VMO, which is what
/// `docs/ARCHITECTURE.md` §7 says the data path is.
pub const CHANNEL_MAX_BYTES: usize = 64 * 1024;

/// The most handles one channel message may carry.
pub const CHANNEL_MAX_HANDLES: usize = 64;

/// A packet on a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct PortPacket {
    /// Whatever the registrant chose, so one waiter can tell its sources
    /// apart.
    pub key: u64,
    /// Which of the `PACKET_*` kinds this is.
    pub kind: u32,
    /// For a signal packet, the signals that were asserted when it fired.
    /// Zero for the other kinds.
    pub signals: u32,
    /// For a user packet, what the sender queued. For an interrupt packet,
    /// the first word is the time it fired in nanoseconds since boot.
    pub data: [u64; 2],
}

/// A packet queued by a program through `port_queue`.
pub const PACKET_USER: u32 = 0;
/// A packet an `object_wait_async` registration produced.
pub const PACKET_SIGNAL: u32 = 1;
/// A packet a bound interrupt produced.
pub const PACKET_INTERRUPT: u32 = 2;

/// `port_fd`'s flag: the descriptor is closed on `execve`, as `O_CLOEXEC`
/// makes one.
pub const PORT_FD_CLOEXEC: u64 = 1;

/// `vmo_map`'s protection: the mapping may be read. Every mapping asks for it.
pub const MAP_READ: u32 = 1 << 0;
/// `vmo_map`'s protection: the mapping may also be written.
pub const MAP_WRITE: u32 = 1 << 1;

/// What a `channel_read` found.
///
/// Written on success, and on `BUFFER_TOO_SMALL` too, when it is how the
/// caller learns what to allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ReadActual {
    /// The message's size in bytes.
    pub bytes: u32,
    /// How many handles it carries.
    pub handles: u32,
}

/// `vmo_pin`'s option: the device may read the pinned pages but not write
/// them.
pub const PIN_READ_ONLY: u64 = 1;

/// `vmo_pin`'s option: the program and the device must see the pages alike,
/// as descriptors a controller polls need. Changes nothing for a device that
/// snoops the caches. For one that does not, such as an STM32MP1's USB host,
/// the pin covers the whole VMO, which nothing may have mapped yet, and every
/// mapping of the VMO from then on bypasses the caches.
pub const PIN_COHERENT: u64 = 2;

/// `vmo_pin`'s option: the pages must be one run of ascending addresses, as
/// NVIDIA's resource manager asks of some system memory it gives the GPU
/// (`docs/NVIDIA.md` §4.3). The range must be wholly uncommitted, in an
/// anonymous VMO, and at most one buddy block long (4 MiB at 4 KiB pages);
/// the kernel fills it with one run of fresh zeroed frames, charged to the
/// caller's job, and refuses with `BAD_STATE`, nothing changed, when what it
/// holds is not one run. Combines with `PIN_READ_ONLY` and `PIN_COHERENT`.
pub const PIN_CONTIGUOUS: u64 = 4;

/// `device_clock`'s option: set the rate, rather than only say what it
/// would be.
pub const CLOCK_SET: u64 = 1;

/// `device_set_limit` and `device_get_limit`'s limit: the device's pin
/// budget, in pages (`docs/NVIDIA.md` §12.2).
pub const DEVICE_LIMIT_PIN_PAGES: u64 = 0;
/// `device_get_limit`'s: the machine's ceiling, in pages, on twice every
/// budget raised above the default -- a quarter of RAM.
pub const DEVICE_LIMIT_PIN_CEILING: u64 = 1;
/// `device_get_limit`'s: the ceiling less twice every budget other devices
/// have raised above the default, in pages: the most twice this device's
/// budget may be raised to.
pub const DEVICE_LIMIT_PIN_ROOM: u64 = 2;

/// `device_set_limit` and `device_get_limit`'s limit: the isolated-interrupts
/// mark, set-once (`docs/NVIDIA.md` §12.3). Set, the kernel refuses the
/// device's vectors and pins while the machine's interrupts are not
/// isolated; `devmgr` sets it for every device that runs firmware of its
/// own, and starts no driver for one it could not mark.
pub const DEVICE_LIMIT_ISOLATED_INTERRUPTS: u64 = 3;

/// `device_isolation`'s bit: an IOMMU translates the device's DMA.
pub const DEVICE_ISOLATION_DMA_TRANSLATED: u64 = 1 << 0;
/// `device_isolation`'s bit: the machine's interrupts are isolated -- every
/// interrupt remapping unit remaps with compatibility format blocked, and
/// no function bypasses them -- and the device's messages are remapped.
pub const DEVICE_ISOLATION_INTERRUPTS: u64 = 1 << 1;

/// `job_set_limit` and `job_get_quota`'s resource: physical memory, in bytes.
pub const JOB_MEMORY: u64 = 0;
/// `job_set_limit` and `job_get_quota`'s resource: kernel objects a program
/// made -- VMOs, channel ends, ports, jobs.
pub const JOB_OBJECTS: u64 = 1;
/// `job_set_limit` and `job_get_quota`'s resource: tasks, a process and each
/// thread beside its first.
pub const JOB_TASKS: u64 = 2;
/// `job_set_limit` and `job_get_quota`'s resource: the job's processor
/// weight, 1 to 10,000.
pub const JOB_CPU_WEIGHT: u64 = 3;
/// `job_set_limit`'s limit that is no limit, and what `job_get_quota` reads
/// for one.
pub const UNLIMITED: u64 = u64::MAX;

/// `job_create`'s option: make the new job one speculation domain, whose
/// programs born in it share predictors with each other, so that a switch
/// between two of them skips the predictor barrier (`docs/OPAQUE-KERNEL.md`
/// §9.2). Only as the job is made; any other bit is `INVALID_ARGS`.
pub const JOB_SPECULATION_DOMAIN: u64 = 1;

/// [`crate::nr::NativeCall::AuditRead`]'s `which`: the high-value ring --
/// grants, ends, device events, changes and the system's records.
pub const AUDIT_HIGH: u64 = 0;

/// [`crate::nr::NativeCall::AuditRead`]'s `which`: the refusal ring.
pub const AUDIT_REFUSALS: u64 = 1;

/// [`crate::nr::NativeCall::AuditRead`]'s `which`: the first eight system
/// records, pinned where nothing else reaches them, numbered as the
/// high-value ring numbered them.
pub const AUDIT_BOOT: u64 = 2;

/// The words [`crate::nr::NativeCall::AuditRead`] writes to its answer:
/// copied, next, lost, and the audit id's low and high halves.
pub const AUDIT_ANSWER_WORDS: usize = 5;

/// Where one of a device's virtio register blocks lies, as `device_info`
/// reports it and a driver's START carries it: the page-aligned physical
/// start of the pages holding it, inside one of the device's apertures, the
/// block's first byte within those pages, and its length. A block the device
/// does not have is all zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct DeviceBlock {
    /// Physical address of the first page, a multiple of the page size.
    pub phys: u64,
    /// The block's first byte, from `phys`.
    pub offset: u32,
    /// Bytes in the block.
    pub length: u32,
}

/// What `device_info` writes: the device as enumeration found it, for
/// whoever starts a driver on it. [`DEVICE_INFO_BYTES`] long, laid out as
/// declared with no padding until the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct DeviceInfo {
    /// virtio's common configuration block.
    pub common: DeviceBlock,
    /// virtio's notification area.
    pub notify: DeviceBlock,
    /// virtio's ISR status block.
    pub isr: DeviceBlock,
    /// virtio's device-specific configuration block.
    pub device: DeviceBlock,
    /// The PCI address as a word (segment in bits 31:16, bus in 15:8, devfn
    /// in 7:0), or [`DEVICE_NOT_PCI`] for a device tree node.
    pub location: u32,
    /// The PCI class code: base class in bits 23:16, subclass in 15:8, the
    /// programming interface in 7:0. Zero for a device tree node.
    pub class: u32,
    /// Apertures the device has, each an `io_mapping_create` can claim and
    /// `device_aperture` describes whole.
    pub apertures: u32,
    /// Vectors the device has, each an `interrupt_create` can claim.
    pub vectors: u32,
    /// virtio's `notify_off_multiplier`.
    pub notify_off_multiplier: u32,
    /// The PCI vendor identifier; zero for a device tree node.
    pub vendor_id: u16,
    /// The PCI device identifier; for a device tree node, the binding
    /// [`DEVICE_TREE_BLOCKS`] names, or zero.
    pub device_id: u16,
    /// MSI-X table entries; zero for a device with a line only.
    pub msix_table_size: u16,
    /// [`DEVICE_VIRTIO_PCI`] when the blocks above describe a virtio PCI
    /// transport, [`DEVICE_TREE_BLOCKS`] when they are a device tree node's
    /// registers, else zero and the blocks are zero.
    pub virtio: u16,
    /// The PCI subsystem vendor identifier; zero for a device tree node or a
    /// bridge. Which machine made a virtio device is told by this pair:
    /// QEMU's are subsystem `0x1af4:0x1100`, crosvm's repeat the device's
    /// own identifier.
    pub subsystem_vendor_id: u16,
    /// The PCI subsystem identifier; zero for a device tree node or a bridge.
    pub subsystem_id: u16,
}

/// Bytes `device_info` writes: a [`DeviceInfo`], padded to its alignment.
///
/// The size is fixed for good: `device_info` takes no length, so a longer
/// struct would be written past the buffer of every driver built before it.
/// What a driver needs beyond it comes from another call, as an aperture's
/// 64-bit address and length come from `device_aperture`. A device tree
/// node's blocks carry `u32` lengths cut at 4 GiB; `device_aperture` has
/// the whole length.
pub const DEVICE_INFO_BYTES: usize = 96;
/// [`DeviceInfo::location`] of a device that is not a PCI function.
pub const DEVICE_NOT_PCI: u32 = u32::MAX;
/// [`DeviceInfo::virtio`] of a virtio PCI transport.
pub const DEVICE_VIRTIO_PCI: u16 = 1;
/// [`DeviceInfo::virtio`] of a device tree node the kernel publishes for a
/// binding it knows: [`DeviceInfo::device_id`] names the binding, the
/// vendor is zero, and the blocks are the node's register windows in the
/// order the binding gives, [`DeviceInfo::common`] first and
/// [`DeviceInfo::device`] second.
pub const DEVICE_TREE_BLOCKS: u16 = 2;
/// [`DeviceInfo::device_id`] of an STM32MP15 DK board's HDMI output: the
/// LTDC's registers in `common`, the HDMI bridge's I2C controller's in
/// `device`, and the LTDC's interrupt as vector 0.
pub const TREE_STM32_HDMI: u16 = 1;
/// [`DeviceInfo::device_id`] of an STM32MP15 board's USB host: the EHCI
/// controller's registers in `common`, nothing in `device`, and the EHCI
/// controller's interrupt as vector 0. Its memory is not snooped, so the
/// driver pins what it shares with `PIN_COHERENT`, and it may make an input
/// control channel for each keyboard or mouse it finds, up to
/// [`USB_INPUT_FUNCTIONS`].
pub const TREE_STM32_USBH: u16 = 2;
/// [`DeviceInfo::device_id`] of an STM32MP157's GPU, a Vivante GC400T: its
/// registers in `common`, nothing in `device`, and its interrupt as vector 0.
/// Its memory is not snooped, so the driver pins what it shares with
/// `PIN_COHERENT`; and while its MMU is off its front end reads one run of
/// physical addresses, so a buffer it reads must be physically contiguous
/// (`docs/GPU.md` §6.3).
pub const TREE_STM32_GPU: u16 = 3;
/// [`DeviceInfo::device_id`] of the Pixel 7's USB device controller, a
/// Synopsys DWC3 on the phone's USB-C port: its registers in `common`,
/// nothing in `device`, and its interrupt as vector 0. Its memory is not
/// snooped, so the driver pins what it shares with `PIN_COHERENT`.
pub const TREE_GS201_DWC3: u16 = 4;
/// How many input control channels one USB host's node may hold at once.
pub const USB_INPUT_FUNCTIONS: usize = 8;

/// What `process_status` writes: whether the process has ended, and how.
/// Eight bytes, the same on every target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ProcessStatus {
    /// [`PROCESS_RUNNING`], [`PROCESS_EXITED`] or [`PROCESS_KILLED`].
    pub state: u32,
    /// Zero while it runs; the exit code, 0 to 255, once it has exited; the
    /// signal's number once one has killed it.
    pub value: u32,
}

/// [`ProcessStatus::state`]: it has not ended. It may not have started.
pub const PROCESS_RUNNING: u32 = 0;
/// [`ProcessStatus::state`]: it ended by `exit` or `exit_group`, or its last
/// thread's `exit`, and [`ProcessStatus::value`] is the code, as `wait4`'s
/// `WEXITSTATUS` would read it.
pub const PROCESS_EXITED: u32 = 1;
/// [`ProcessStatus::state`]: a signal ended it -- one sent to it, its own
/// fault, or a kill of its job, which is `SIGKILL` -- and
/// [`ProcessStatus::value`] is the signal, as `WTERMSIG` would read it.
pub const PROCESS_KILLED: u32 = 2;

/// What `device_aperture` writes: one of the device's apertures as
/// enumeration minted it. [`APERTURE_INFO_BYTES`] long, laid out as declared
/// with no padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ApertureInfo {
    /// The physical address of its first byte.
    pub phys: u64,
    /// Its length in bytes, whole, however far past 4 GiB.
    pub len: u64,
    /// The BAR it came from, or [`APERTURE_NOT_BAR`] for a device tree
    /// node's window. One BAR can give two apertures, either side of the
    /// MSI-X table's pages, and a BAR whose memory the kernel withheld gives
    /// none, so this is not the index.
    pub bar: u8,
    /// [`APERTURE_PREFETCHABLE`], [`APERTURE_WHOLE_PAGES`] and
    /// [`APERTURE_BAR_64`], as they hold.
    pub flags: u8,
    /// Written zero.
    pub reserved: [u8; 6],
    /// Where in its BAR it starts, in bytes; zero for a tree window.
    pub offset: u64,
}

/// Bytes `device_aperture` writes.
pub const APERTURE_INFO_BYTES: usize = 32;
/// [`ApertureInfo::bar`] of an aperture that is a device tree window.
pub const APERTURE_NOT_BAR: u8 = 0xFF;
/// [`ApertureInfo::flags`]: reads have no side effects, so it may be mapped
/// write-combining (`io_mapping_map_combining`).
pub const APERTURE_PREFETCHABLE: u8 = 1 << 0;
/// [`ApertureInfo::flags`]: it starts on a page and is whole pages, so
/// `io_mapping_create` can claim it whole.
pub const APERTURE_WHOLE_PAGES: u8 = 1 << 1;
/// [`ApertureInfo::flags`]: its BAR is 64 bits wide.
pub const APERTURE_BAR_64: u8 = 1 << 2;

/// The aperture an `io_mapping_create` claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct IoMappingSpec {
    /// The physical address of the first byte.
    pub phys: u64,
    /// The length in bytes.
    pub len: u64,
}

/// `chardev_dmabuf_install`'s flags: the descriptor is open for reading and
/// writing, so a program may map it writable. Needs `WRITE` on the VMO;
/// without it the descriptor is read-only.
pub const DMABUF_WRITABLE: u64 = 1 << 0;
/// `chardev_dmabuf_install`'s flags: the descriptor is close-on-exec.
pub const DMABUF_CLOEXEC: u64 = 1 << 1;
/// `chardev_dmabuf_install`'s flags: set [`DMABUF_MADE`] in the answer when
/// the call made the dmabuf rather than finding one alive for the cookie.
///
/// What lets a driver count dmabuf objects per cookie, one up for each made
/// and one down for each `DMABUF_RELEASE`. Without it a driver cannot tell
/// a dmabuf found alive from one made just after the last went, whose
/// RELEASE may still be on its way: the release of the old one would then
/// read as the release of the new.
pub const DMABUF_TELL_MADE: u64 = 1 << 2;
/// Every flag `chardev_dmabuf_install` knows; any other is `INVALID_ARGS`.
pub const DMABUF_FLAGS: u64 = DMABUF_WRITABLE | DMABUF_CLOEXEC | DMABUF_TELL_MADE;
/// In `chardev_dmabuf_install`'s answer, with [`DMABUF_TELL_MADE`]: this call
/// made the dmabuf. Above every descriptor number, which is at most
/// `i32::MAX`.
pub const DMABUF_MADE: usize = 1 << 31;
