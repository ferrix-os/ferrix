//! The native system call numbers.
//!
//! Grouped in blocks of eight by object, with gaps, so a call added to a
//! family later lands next to its siblings rather than at the end of the
//! table. A number, once assigned, is never reused for a different call.
//!
//! Each call's arguments are listed on its [`NativeCall`] variant in register
//! order. `*u64` means a pointer to a 64-bit value in the caller's memory, for
//! the reason the crate documentation gives.

/// The first native number.
pub const FIRST: usize = 0x1000;
/// The last number the native range may ever use.
pub const LAST: usize = 0x1FFF;

/// [`NativeCall::HandleClose`].
pub const HANDLE_CLOSE: usize = 0x1000;
/// [`NativeCall::HandleDuplicate`].
pub const HANDLE_DUPLICATE: usize = 0x1001;
/// [`NativeCall::HandleReplace`].
pub const HANDLE_REPLACE: usize = 0x1002;

/// [`NativeCall::ObjectWaitOne`].
pub const OBJECT_WAIT_ONE: usize = 0x1008;
/// [`NativeCall::ObjectWaitAsync`].
pub const OBJECT_WAIT_ASYNC: usize = 0x1009;

/// [`NativeCall::ChannelCreate`].
pub const CHANNEL_CREATE: usize = 0x1010;
/// [`NativeCall::ChannelWrite`].
pub const CHANNEL_WRITE: usize = 0x1011;
/// [`NativeCall::ChannelRead`].
pub const CHANNEL_READ: usize = 0x1012;
/// [`NativeCall::ChannelWriteRead`].
pub const CHANNEL_WRITE_READ: usize = 0x1013;
/// The most bytes a [`NativeCall::ChannelWriteRead`] carries each way: three
/// words, which travel in registers.
pub const CHANNEL_WRITE_READ_BYTES: usize = 3 * size_of::<usize>();
/// [`NativeCall::ChannelWriteRead`]'s `count` for a call that only receives.
pub const WRITE_READ_NOTHING: usize = usize::MAX;

/// [`NativeCall::PortCreate`].
pub const PORT_CREATE: usize = 0x1018;
/// [`NativeCall::PortQueue`].
pub const PORT_QUEUE: usize = 0x1019;
/// [`NativeCall::PortWait`].
pub const PORT_WAIT: usize = 0x101A;
/// [`NativeCall::PortFd`].
pub const PORT_FD: usize = 0x101B;

/// [`NativeCall::VmoCreate`].
pub const VMO_CREATE: usize = 0x1020;
/// [`NativeCall::VmoRead`].
pub const VMO_READ: usize = 0x1021;
/// [`NativeCall::VmoWrite`].
pub const VMO_WRITE: usize = 0x1022;
/// [`NativeCall::VmoGetSize`].
pub const VMO_GET_SIZE: usize = 0x1023;
/// [`NativeCall::VmoMap`].
pub const VMO_MAP: usize = 0x1024;
/// [`NativeCall::VmoPin`].
pub const VMO_PIN: usize = 0x1025;
/// [`NativeCall::VmoPinAddresses`].
pub const VMO_PIN_ADDRESSES: usize = 0x1026;

/// [`NativeCall::JobCreate`].
pub const JOB_CREATE: usize = 0x1028;
/// [`NativeCall::JobKill`].
pub const JOB_KILL: usize = 0x1029;
/// [`NativeCall::JobForCgroup`].
pub const JOB_FOR_CGROUP: usize = 0x102A;
/// [`NativeCall::JobSetLimit`].
pub const JOB_SET_LIMIT: usize = 0x102B;
/// [`NativeCall::JobGetQuota`].
pub const JOB_GET_QUOTA: usize = 0x102C;

/// [`NativeCall::ProcessCreate`].
pub const PROCESS_CREATE: usize = 0x1030;
/// [`NativeCall::ProcessStart`].
pub const PROCESS_START: usize = 0x1031;
/// [`NativeCall::ProcessGive`].
pub const PROCESS_GIVE: usize = 0x1032;
/// [`NativeCall::ProcessBootstrap`].
pub const PROCESS_BOOTSTRAP: usize = 0x1033;
/// [`NativeCall::ProcessStatus`].
pub const PROCESS_STATUS: usize = 0x1034;

/// [`NativeCall::InterruptCreate`].
pub const INTERRUPT_CREATE: usize = 0x1038;
/// [`NativeCall::InterruptBind`].
pub const INTERRUPT_BIND: usize = 0x1039;
/// [`NativeCall::InterruptAck`].
pub const INTERRUPT_ACK: usize = 0x103A;

/// [`NativeCall::IoMappingCreate`].
pub const IO_MAPPING_CREATE: usize = 0x1040;
/// [`NativeCall::IoMappingMap`].
pub const IO_MAPPING_MAP: usize = 0x1041;
/// [`NativeCall::IoMappingMapCombining`].
pub const IO_MAPPING_MAP_COMBINING: usize = 0x1042;

/// [`NativeCall::BlockRingCreate`].
pub const BLOCK_RING_CREATE: usize = 0x1048;

/// [`NativeCall::NetRingCreate`].
pub const NET_RING_CREATE: usize = 0x104B;
/// [`NativeCall::DisplayControlCreate`].
pub const DISPLAY_CONTROL_CREATE: usize = 0x104C;
/// [`NativeCall::InputControlCreate`].
pub const INPUT_CONTROL_CREATE: usize = 0x104D;
/// [`NativeCall::RenderControlCreate`].
pub const RENDER_CONTROL_CREATE: usize = 0x104E;
/// [`NativeCall::DeviceInfo`].
pub const DEVICE_INFO: usize = 0x1049;
/// [`NativeCall::DeviceQuiesce`].
pub const DEVICE_QUIESCE: usize = 0x104A;
/// [`NativeCall::DeviceClock`].
pub const DEVICE_CLOCK: usize = 0x104F;
/// [`NativeCall::SoundControlCreate`].
pub const SOUND_CONTROL_CREATE: usize = 0x1050;
/// [`NativeCall::LogControlCreate`].
pub const LOG_CONTROL_CREATE: usize = 0x1051;
/// [`NativeCall::DevmgrStart`].
pub const DEVMGR_START: usize = 0x1052;
/// [`NativeCall::AuditRead`].
pub const AUDIT_READ: usize = 0x1053;
/// [`NativeCall::DeviceAperture`].
pub const DEVICE_APERTURE: usize = 0x1054;
/// [`NativeCall::DeviceConfigRead`].
pub const DEVICE_CONFIG_READ: usize = 0x1055;
/// [`NativeCall::DeviceConfigWrite`].
pub const DEVICE_CONFIG_WRITE: usize = 0x1056;
/// [`NativeCall::DeviceSetLimit`].
pub const DEVICE_SET_LIMIT: usize = 0x1057;
/// [`NativeCall::DeviceGetLimit`].
pub const DEVICE_GET_LIMIT: usize = 0x1058;
/// [`NativeCall::DeviceIsolation`].
pub const DEVICE_ISOLATION: usize = 0x1059;
/// [`NativeCall::ChardevControlCreate`].
pub const CHARDEV_CONTROL_CREATE: usize = 0x105A;
/// [`NativeCall::ChardevReply`].
pub const CHARDEV_REPLY: usize = 0x105B;
/// [`NativeCall::ChardevCopyIn`].
pub const CHARDEV_COPY_IN: usize = 0x105C;
/// [`NativeCall::ChardevCopyOut`].
pub const CHARDEV_COPY_OUT: usize = 0x105D;
/// The most records one [`NativeCall::AuditRead`] copies.
pub const AUDIT_READ_MAX: u64 = 64;
/// The largest name [`NativeCall::ProcessCreate`] takes, in bytes.
pub const PROCESS_NAME_MAX: usize = 32;

/// A native system call.
///
/// `0x1035..=0x1037` is left for the calls that act on a process beyond
/// making, starting, giving it its bootstrap and reading how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeCall {
    /// `(handle)`. Close a handle. The object lives on if anything else holds it.
    HandleClose,
    /// `(handle, rights)` → handle. A second handle with the same or fewer
    /// rights. Needs `DUPLICATE`.
    HandleDuplicate,
    /// `(handle, rights)` → handle. Swap a handle for one with the same or
    /// fewer rights; the original is closed. Needs no right, because it can
    /// only give something up.
    HandleReplace,
    /// `(handle, signals, deadline: *u64, observed: *u32)`. Block until any of
    /// `signals` is asserted or the deadline passes, then write the signals
    /// asserted at that moment. The deadline is absolute, in `CLOCK_MONOTONIC`
    /// nanoseconds; a null pointer waits forever, and a null `observed` is
    /// not written. Needs `WAIT`.
    ObjectWaitOne,
    /// `(handle, port, signals, key: *u64)`. Queue one packet on `port`, with
    /// `key`, the next time any of `signals` is asserted — at once, if one
    /// already is. One-shot. Needs `WAIT` on the object and `WRITE` on the port.
    ObjectWaitAsync,
    /// `(out: *[u32; 2])`. Make a channel, writing its two endpoints' handles.
    ChannelCreate,
    /// `(channel, bytes, byte_count, handles: *u32, handle_count)`. Send one
    /// message. The handles leave this process only if the write succeeds.
    /// Needs `WRITE`, and `TRANSFER` on every handle sent.
    ChannelWrite,
    /// `(channel, bytes, byte_capacity, handles: *u32, handle_capacity,
    /// actual: *ReadActual)`. Receive one message. If it does not fit, it stays
    /// queued and `actual` reports what it needs. Needs `READ`.
    ChannelRead,
    /// `(channel, count, w0, w1, w2)` → count. Send `count` bytes, at most
    /// [`CHANNEL_WRITE_READ_BYTES`], taken from the words `w0`, `w1`, `w2` as
    /// they would lie in memory, unless `count` is [`WRITE_READ_NOTHING`];
    /// then block until a message is readable on the same end, and receive
    /// it the same way if it carries no handles and fits in three words. The
    /// answer is its size in bytes, and its words come back in the second,
    /// third and fourth argument registers. One call where a round trip took
    /// a write, a wait and a read, and no copy through the program's memory:
    /// seL4's register messages under Zircon's `channel_call`. `PEER_CLOSED`
    /// when nothing more will come; `BUFFER_TOO_SMALL` for a message that
    /// needs [`NativeCall::ChannelRead`], left queued; `TIMED_OUT` never, as
    /// the call waits as long as it takes; and whatever a write is refused
    /// for, before it waits. The end of the caller's process, or another
    /// thread's `execve`, ends the wait with `EINTR`; a signal does not, as it
    /// ends no native wait. Needs `WRITE` to send and `READ` to receive.
    ChannelWriteRead,
    /// `()` → handle. Make a port.
    PortCreate,
    /// `(port, packet: *PortPacket)`. Queue a user packet. Needs `WRITE`.
    PortQueue,
    /// `(port, deadline: *u64, packet: *PortPacket)`. Take the next packet,
    /// waiting for one up to the deadline. Needs `READ`.
    PortWait,
    /// `(port, flags)` → descriptor. A Linux file descriptor on the port,
    /// which `poll`, `select` and `epoll` report readable (`POLLIN`,
    /// `EPOLLIN`) while the port has a packet queued and not otherwise, and
    /// which wakes a waiter on it whenever a packet is queued: how a program
    /// that waits in `epoll_wait` hears a port (`docs/INIT.md` §9, K4). The
    /// descriptor only reports: packets are taken with `port_wait`, and a
    /// `read` or `write` of it is `EINVAL`. It keeps the port alive while it
    /// is open, whatever becomes of the handle. `flags` is
    /// [`crate::types::PORT_FD_CLOEXEC`] or zero. Needs `WAIT`; `INVALID_ARGS`
    /// for another flag, `NO_HANDLES` (`EMFILE`) when the descriptor table is
    /// full.
    PortFd,
    /// `(bytes)` → handle. Make an anonymous VMO, rounded up to whole pages.
    VmoCreate,
    /// `(vmo, buffer, count, offset: *u64)`. Copy out of a VMO. Needs `READ`.
    VmoRead,
    /// `(vmo, buffer, count, offset: *u64)`. Copy into a VMO. Needs `WRITE`.
    VmoWrite,
    /// `(vmo, size: *u64)`. The VMO's size in bytes.
    VmoGetSize,
    /// `(vmo, address, length, protection, offset: *u64)` → address. Map
    /// `length` bytes of a VMO from `offset`, both whole pages, at `address`,
    /// or wherever there is room if it is zero. Shared: a write through the
    /// mapping is a write to the VMO. `protection` is `MAP_READ`, or
    /// `MAP_READ | MAP_WRITE`; nothing maps a VMO executable. Needs `MAP`,
    /// and `READ`/`WRITE` for the protection asked for.
    VmoMap,
    /// `(device, vmo, offset, length, options)` → handle. Pin a range of a VMO
    /// into the device's IOMMU domain, so the device may reach it, and hold
    /// its pages until the handle is closed. The range is whole pages, and
    /// `options` is [`crate::types::PIN_READ_ONLY`],
    /// [`crate::types::PIN_COHERENT`] or zero. Needs `MANAGE` on the device,
    /// `READ` on the VMO, and `WRITE` unless read-only. `VmoRead` and
    /// `VmoWrite` refuse a VMO a coherent pin has marked, with `BAD_STATE`.
    VmoPin,
    /// `(pin, addresses: *u64, capacity)` → pages. Write each pinned page's
    /// device address, in page order, up to `capacity`; the answer is how many
    /// pages the pin holds, whatever fits. Needs `READ`.
    VmoPinAddresses,
    /// `(parent, options)` → handle. Make a job inside `parent`. Needs
    /// `MANAGE`. `options` is zero, or [`crate::types::JOB_SPECULATION_DOMAIN`]
    /// to make the job one speculation domain; any other bit is
    /// `INVALID_ARGS`.
    JobCreate,
    /// `(job)`. End every process in the job and every job inside it. Needs
    /// `MANAGE`.
    JobKill,
    /// `(dirfd, rights)` → handle. The job behind the cgroupfs directory the
    /// descriptor `dirfd` is open on (`docs/CGROUPS.md` §5): the one bridge
    /// from a cgroup path to a job handle, and one way only. The handle
    /// carries `DUPLICATE` and `TRANSFER`, `WAIT` if the caller may read that
    /// directory's `cgroup.procs`, and `MANAGE` too if it may write it;
    /// `rights` asks for a subset of those, or for all of them with
    /// [`crate::rights::SAME_RIGHTS`]. `BAD_HANDLE` for a descriptor not
    /// open, `WRONG_TYPE` for one that is not a cgroup directory,
    /// `ACCESS_DENIED` for rights past the caller's access, `BAD_STATE` for a
    /// cgroup `rmdir` removed.
    JobForCgroup,
    /// `(job, resource, limit: *u64)`. Limit what the job and every job
    /// inside it may hold at once of `resource`, one of
    /// [`crate::types::JOB_MEMORY`] (bytes, rounded down to pages),
    /// [`crate::types::JOB_OBJECTS`] and [`crate::types::JOB_TASKS`], or set
    /// its processor weight, [`crate::types::JOB_CPU_WEIGHT`] (1 to 10,000;
    /// 100 is a task's). [`crate::types::UNLIMITED`] lifts a limit. A limit
    /// below what the job holds takes nothing away and refuses every charge
    /// until it holds less. A charge past a limit is refused with `NO_MEMORY`
    /// for memory and objects and `SHOULD_WAIT` for tasks. `INVALID_ARGS` for
    /// another resource or a weight out of range, `BAD_STATE` for the root
    /// job, which nothing limits. Needs `MANAGE`; a program is bounded by a
    /// limit on a job above any it holds a handle to.
    JobSetLimit,
    /// `(job, resource, out: *[u64; 3])`. What the job and every job inside
    /// it hold of `resource` now, its limit, and how many charges that limit
    /// has refused, in that order; for [`crate::types::JOB_CPU_WEIGHT`], zero,
    /// the weight and zero. The root job holds nothing and has no limit.
    /// Needs `WAIT`.
    JobGetQuota,
    /// `(job, image, name, name_len)` → handle. Make a process in `job` from
    /// the ELF image the VMO holds, not yet running: the image is read out of
    /// the VMO and loaded as the process's own memory, with nothing on its
    /// stack. `name`, at most [`PROCESS_NAME_MAX`] bytes, is what it is listed
    /// as. The handle carries [`crate::rights::Rights::PROCESS`]; closing the
    /// last one to a process nobody started ends it. Needs `MANAGE` on the job
    /// and `READ` on the VMO.
    ProcessCreate,
    /// `(process, bootstrap)`. Start a process made by `process_create`,
    /// moving `bootstrap` into its table and entering it with that handle's
    /// value in its first argument register; zero starts it with no handle and
    /// zero there. Refused with `BAD_STATE` for a process already started or
    /// already ended, and the bootstrap then stays with the caller. Needs
    /// `MANAGE` on the process and `TRANSFER` on the bootstrap.
    ProcessStart,
    /// `(pid, handle)`. Move `handle` out of the caller's table and hold it
    /// as the bootstrap handle of `pid`, which must be the caller's own child
    /// and must not yet have completed an `execve`: how a Linux program that
    /// starts a native-aware one with `fork` and `execve` gives it a channel
    /// (`docs/INIT.md` §6, K3). The child takes it with
    /// [`NativeCall::ProcessBootstrap`], before or after its `execve`; it is
    /// kept across every `execve` until then, and closed if the child ends
    /// first. One give per process, ever. Needs `TRANSFER` on the handle.
    /// Refused, with the handle left where it was: `BAD_HANDLE` for a handle
    /// not held, `ACCESS_DENIED` without `TRANSFER`, `NO_PROCESS` for a pid
    /// that names no live process (or names a thread rather than a process),
    /// `NOT_CHILD` for a process that is not the caller's child,
    /// `ALREADY_BOUND` for a child given one before (taken or not), and
    /// `BAD_STATE` for a child that has completed an `execve` or has ended.
    ProcessGive,
    /// `()` → handle. The calling process's bootstrap handle, moved into its
    /// table: the one [`NativeCall::ProcessGive`] or the kernel gave it. Once:
    /// every later call answers zero, which is never a handle, and so does a
    /// call from a process given none. `NO_HANDLES` with a full table, and
    /// the handle is kept for a later call. Callable from any process, a
    /// Linux one by `syscall(0x1033)`.
    ProcessBootstrap,
    /// `(process, out: *ProcessStatus)`. Write how the process the handle
    /// names ended, or that it has not (`docs/INIT.md` §11, K6): a
    /// [`crate::types::ProcessStatus`] whose `state` is
    /// [`crate::types::PROCESS_RUNNING`], [`crate::types::PROCESS_EXITED`]
    /// with the exit code, or [`crate::types::PROCESS_KILLED`] with the
    /// signal that ended it. Ended means ended: the answer changes from
    /// running at the moment the process starts to end, which is before the
    /// handle asserts `TERMINATED`, so a waiter woken by `TERMINATED` always
    /// reads an end. Needs `WAIT`; `WRONG_TYPE` for a handle to anything but
    /// a process, `FAULT` for `out`.
    ProcessStatus,
    /// `(resource, vector)` → handle. Claim a hardware interrupt.
    InterruptCreate,
    /// `(interrupt, port, key: *u64)`. Deliver the interrupt to a port as
    /// packets carrying `key`. Needs `MANAGE`.
    InterruptBind,
    /// `(interrupt)`. Re-arm an interrupt after servicing it. Needs `MANAGE`.
    InterruptAck,
    /// `(resource, spec: *IoMappingSpec)` → handle. Claim an MMIO aperture.
    IoMappingCreate,
    /// `(mapping, address)` → address. Map an aperture. Needs `MAP`.
    IoMappingMap,
    /// `(mapping, address)` → address. Map an aperture write-combining, so
    /// the processor may gather stores into bursts: a GPU's aperture onto
    /// its own memory (`docs/NVIDIA.md` §4.3). Needs `MAP`, and an aperture
    /// the device says is prefetchable; `INVALID_ARGS` otherwise.
    IoMappingMapCombining,
    /// `(device)` → handle. Make the block ring a driver serves the device's
    /// disk through (`docs/BLOCK-RING.md`): the kernel keeps one end of the
    /// ring's control channel and answers with the other, on which the driver
    /// sends HELLO. Needs `MANAGE` on the device, which has one ring.
    BlockRingCreate,
    /// Make a net ring for a device this process holds with `MANAGE`, and
    /// answer the driver's end of its control channel.
    NetRingCreate,
    /// `(device)` → handle. Make the display control channel for a device this
    /// process holds with `MANAGE`, and answer the driver's end of it
    /// (`docs/DISPLAY.md` §2.2). One per device.
    DisplayControlCreate,
    /// `(device)` → handle. Make the input control channel for a device this
    /// process holds with `MANAGE`, and answer the driver's end of it
    /// (`docs/INPUT.md` §3.2). One per device.
    InputControlCreate,
    /// `(device)` → handle. Make the *render* control channel for a device
    /// this process holds with `MANAGE`, and answer the driver's end of it
    /// (`docs/GPU.md` §3.3). One per device, and separate from the display's:
    /// a card has two conversations, one about what is on the screen and one
    /// about what the GPU computes, and only the second has a second driver
    /// coming.
    RenderControlCreate,
    /// `(device, info)` → 0. Write a `DeviceInfo` at `info`: the device as
    /// enumeration found it, which is what whoever starts a driver on it puts
    /// in the driver's START. Any device handle will do.
    DeviceInfo,
    /// `(device)` → 0. The device's driver is gone and nothing else will
    /// reach the device: turn its bus mastering off and release its block
    /// ring, so the next driver can have it. Needs `MANAGE`; `BAD_STATE`
    /// while a driver still serves it.
    DeviceQuiesce,
    /// `(device, hz, options)` → hz. The rate nearest `hz` the kernel will
    /// run the device's pixel clock at, and with `CLOCK_SET`, that rate set.
    /// The clock is the one thing about a display the kernel keeps and its
    /// driver needs changed per mode: on an STM32MP15 DK board the divider
    /// of PLL4's Q output, which the kernel changes only while nothing else
    /// runs from it (`docs/DISPLAY.md` §6). Needs `MANAGE`; `WRONG_TYPE` for
    /// a device with no such clock, `BAD_STATE` when the clock cannot be set
    /// now.
    DeviceClock,
    /// `(device)` → handle. Make the sound control channel for a device this
    /// process holds with `MANAGE`, and answer the driver's end of it
    /// (`docs/AUDIO.md` §3.2). One per device.
    SoundControlCreate,
    /// `(device)` → handle. Make a control channel on which the device's
    /// driver reads the kernel log (`src/lib/proto/logctl`), and answer the driver's
    /// end of it. Needs `MANAGE` on a device whose binding may stream the log
    /// off the machine -- the Pixel 7's USB device controller -- and
    /// `ACCESS_DENIED` for any other; one reader at a time, `ALREADY_BOUND`
    /// while another holds it, and the claim ends when the channel closes.
    LogControlCreate,
    /// `(starter, job)` → handle. Ask the kernel to start `devmgr` in `job`
    /// (`docs/INIT.md` §7.3, L12): the kernel loads `/sbin/devmgr` itself,
    /// writes its DEVICES messages, and makes and starts it in `job` with
    /// that channel as its bootstrap, keeping the other end. The caller gets
    /// a handle to the process, with [`crate::rights::Rights::PROCESS`], which
    /// reaches nothing inside it; the channel never passes through the
    /// caller. `starter` is the one-shot capability the kernel gives pid 1
    /// on its bootstrap channel when the command line says
    /// `ferrix.devmgr=init` (needs `MANAGE`); `job` needs `MANAGE`.
    /// `ALREADY_BOUND` while a `devmgr` it started lives; `BAD_STATE` after
    /// one ended, until every driver it started has ended too.
    DevmgrStart,
    /// `audit_read(audit, which, buffer, count, answer)`: copy the audit
    /// record's whole records of ring `which` ([`crate::types::AUDIT_HIGH`],
    /// [`crate::types::AUDIT_REFUSALS`] or [`crate::types::AUDIT_BOOT`])
    /// numbered `from` or later into `buffer`, 64 bytes each as
    /// `src/lib/proto/audit` lays them out, at most `count` and at most
    /// [`AUDIT_READ_MAX`]. `answer` is [`crate::types::AUDIT_ANSWER_WORDS`]
    /// 64-bit words in the machine's order, and its second word is `from` on
    /// the way in, so that a sequence number is 64 bits on every
    /// architecture; the kernel then writes all five: how many were copied,
    /// the number to read from next, how many between `from` and the first
    /// copied the ring no longer held, and the boot's 128-bit audit id, low
    /// word first. A `count` of zero copies nothing and answers `from` as
    /// the next number. Needs `READ` on an audit handle, which only pid 1 is
    /// given (`docs/certification/AUDIT.md` §4). Never blocks: a reader
    /// polls.
    AuditRead,
    /// `(device, index, info: *ApertureInfo)` → 0. Write the device's
    /// aperture `index` as an [`crate::types::ApertureInfo`]: its whole
    /// 64-bit address and length, the BAR it came from and where in it, and
    /// whether it is prefetchable, whole pages and from a 64-bit BAR
    /// (`docs/NVIDIA.md` §12.1). `index` runs below
    /// [`crate::types::DeviceInfo::apertures`]; past it, `INVALID_ARGS`. Any
    /// device handle will do, as for `DeviceInfo`; `FAULT` for `info`.
    DeviceAperture,
    /// `(device, offset, width, value: *u32)` → 0. Read `width` bytes, 1, 2
    /// or 4, of the PCI function's configuration space at `offset`, a
    /// multiple of `width` below 4096, into the low bytes of `value`. Every
    /// byte is readable, the kernel's registers included; a function with
    /// only 256 bytes reads all ones above them. `INVALID_ARGS` for another
    /// width or offset, `WRONG_TYPE` for a device that is not a PCI function
    /// with configuration space, `BAD_STATE` for one the kernel refused.
    /// Needs `MANAGE`.
    DeviceConfigRead,
    /// `(device, offset, width, value)` → 0. Write the low `width` bytes of
    /// `value`, under the same rules of width and offset as
    /// [`NativeCall::DeviceConfigRead`]. Only a write every byte of which
    /// lies in the function's driver-writable ranges -- the bodies of its
    /// vendor-specific capabilities, virtio's `pci_cfg_data` excepted -- is
    /// made; any other is `ACCESS_DENIED` and writes nothing. Needs `MANAGE`.
    DeviceConfigWrite,
    /// `(device, which, value)` → 0. Set one of the device's limits:
    /// [`crate::types::DEVICE_LIMIT_ISOLATED_INTERRUPTS`], set-once -- with a
    /// value other than 0 it marks the device so that `interrupt_create`
    /// and `vmo_pin` are `ACCESS_DENIED` for it while the machine's
    /// interrupts are not isolated, and a value of 0 on a marked device is
    /// `ACCESS_DENIED` (`docs/NVIDIA.md` §12.3); or
    /// [`crate::types::DEVICE_LIMIT_PIN_PAGES`], its pin budget in
    /// pages (`docs/NVIDIA.md` §12.2): a pin that would take the pages its
    /// device's live pins hold past it is refused `LIMIT_REACHED`, and one
    /// that would take what its quarantine, its kept pins and its live pins
    /// hold past twice it is refused `QUARANTINE_FULL`. Needs `SET_LIMIT`,
    /// which `devmgr` holds and never hands a driver. `BAD_STATE` while the
    /// device has live pins, so a budget never changes under a driver;
    /// `NO_MEMORY`, with nothing changed, when budgets raised above the
    /// default would then hold more than the kernel's ceiling;
    /// `INVALID_ARGS` for another `which`.
    DeviceSetLimit,
    /// `(device, which)` → value. Read one of the device's limits:
    /// [`crate::types::DEVICE_LIMIT_PIN_PAGES`], its pin budget;
    /// [`crate::types::DEVICE_LIMIT_PIN_CEILING`], the machine's ceiling on
    /// raised budgets; [`crate::types::DEVICE_LIMIT_PIN_ROOM`], what of the
    /// ceiling other devices' raised budgets leave;
    /// [`crate::types::DEVICE_LIMIT_ISOLATED_INTERRUPTS`], 1 when the device
    /// is marked. Any device handle will do; `INVALID_ARGS` for another
    /// `which`.
    DeviceGetLimit,
    /// `(device)` → bits. How the device is isolated:
    /// [`crate::types::DEVICE_ISOLATION_DMA_TRANSLATED`] when an IOMMU
    /// translates its DMA, and
    /// [`crate::types::DEVICE_ISOLATION_INTERRUPTS`] when the machine's
    /// interrupts are isolated and its own messages remapped, so it raises
    /// only the vectors minted for it (`H.DMA.9`). Any device handle will
    /// do.
    DeviceIsolation,
    /// `(device)` → channel. The device's chardev control channel, one per
    /// device, for a driver whose device files the kernel forwards
    /// undecoded (`docs/NVIDIA.md` §4.4): HELLO on it lists the minors of
    /// major 195 it serves, and REQUESTs come back. The driver's end has no
    /// `TRANSFER` or `DUPLICATE`. Needs `MANAGE` on a device whose
    /// interrupts are isolated. Answered by the load ring.
    ChardevControlCreate,
    /// `(control, request, status, value)`. Answer an outstanding request:
    /// `status` zero or a negative errno, `value` an ioctl's return.
    /// `BAD_STATE` for a request that is not outstanding on this control.
    /// Answered by the load ring.
    ChardevReply,
    /// `(control, request, client address, buffer, length)`. Copy from the
    /// memory of the program waiting in the request into the caller's
    /// buffer. Only while the request is outstanding. Answered by the load
    /// ring.
    ChardevCopyIn,
    /// `(control, request, client address, buffer, length)`. The other way.
    /// Answered by the load ring.
    ChardevCopyOut,
}

/// Every native call, in number order.
pub const ALL: [NativeCall; 58] = [
    NativeCall::HandleClose,
    NativeCall::HandleDuplicate,
    NativeCall::HandleReplace,
    NativeCall::ObjectWaitOne,
    NativeCall::ObjectWaitAsync,
    NativeCall::ChannelCreate,
    NativeCall::ChannelWrite,
    NativeCall::ChannelRead,
    NativeCall::ChannelWriteRead,
    NativeCall::PortCreate,
    NativeCall::PortQueue,
    NativeCall::PortWait,
    NativeCall::PortFd,
    NativeCall::VmoCreate,
    NativeCall::VmoRead,
    NativeCall::VmoWrite,
    NativeCall::VmoGetSize,
    NativeCall::VmoMap,
    NativeCall::VmoPin,
    NativeCall::VmoPinAddresses,
    NativeCall::JobCreate,
    NativeCall::JobKill,
    NativeCall::JobForCgroup,
    NativeCall::JobSetLimit,
    NativeCall::JobGetQuota,
    NativeCall::ProcessCreate,
    NativeCall::ProcessStart,
    NativeCall::ProcessGive,
    NativeCall::ProcessBootstrap,
    NativeCall::ProcessStatus,
    NativeCall::InterruptCreate,
    NativeCall::InterruptBind,
    NativeCall::InterruptAck,
    NativeCall::IoMappingCreate,
    NativeCall::IoMappingMap,
    NativeCall::IoMappingMapCombining,
    NativeCall::BlockRingCreate,
    NativeCall::DeviceInfo,
    NativeCall::DeviceQuiesce,
    NativeCall::NetRingCreate,
    NativeCall::DisplayControlCreate,
    NativeCall::InputControlCreate,
    NativeCall::RenderControlCreate,
    NativeCall::DeviceClock,
    NativeCall::SoundControlCreate,
    NativeCall::LogControlCreate,
    NativeCall::DevmgrStart,
    NativeCall::AuditRead,
    NativeCall::DeviceAperture,
    NativeCall::DeviceConfigRead,
    NativeCall::DeviceConfigWrite,
    NativeCall::DeviceSetLimit,
    NativeCall::DeviceGetLimit,
    NativeCall::DeviceIsolation,
    NativeCall::ChardevControlCreate,
    NativeCall::ChardevReply,
    NativeCall::ChardevCopyIn,
    NativeCall::ChardevCopyOut,
];

/// Whether `number` is in the native range at all.
///
/// The dispatcher asks this before any Linux table, which is what keeps the
/// two ABIs from ever having to agree on a number.
#[must_use]
pub const fn is_native(number: usize) -> bool {
    number >= FIRST && number <= LAST
}

/// The call a number names, or `None` for a gap.
#[must_use]
pub const fn decode(number: usize) -> Option<NativeCall> {
    let call = match number {
        HANDLE_CLOSE => NativeCall::HandleClose,
        HANDLE_DUPLICATE => NativeCall::HandleDuplicate,
        HANDLE_REPLACE => NativeCall::HandleReplace,
        OBJECT_WAIT_ONE => NativeCall::ObjectWaitOne,
        OBJECT_WAIT_ASYNC => NativeCall::ObjectWaitAsync,
        CHANNEL_CREATE => NativeCall::ChannelCreate,
        CHANNEL_WRITE => NativeCall::ChannelWrite,
        CHANNEL_READ => NativeCall::ChannelRead,
        CHANNEL_WRITE_READ => NativeCall::ChannelWriteRead,
        PORT_CREATE => NativeCall::PortCreate,
        PORT_QUEUE => NativeCall::PortQueue,
        PORT_WAIT => NativeCall::PortWait,
        PORT_FD => NativeCall::PortFd,
        VMO_CREATE => NativeCall::VmoCreate,
        VMO_READ => NativeCall::VmoRead,
        VMO_WRITE => NativeCall::VmoWrite,
        VMO_GET_SIZE => NativeCall::VmoGetSize,
        VMO_MAP => NativeCall::VmoMap,
        VMO_PIN => NativeCall::VmoPin,
        VMO_PIN_ADDRESSES => NativeCall::VmoPinAddresses,
        JOB_CREATE => NativeCall::JobCreate,
        JOB_KILL => NativeCall::JobKill,
        JOB_FOR_CGROUP => NativeCall::JobForCgroup,
        JOB_SET_LIMIT => NativeCall::JobSetLimit,
        JOB_GET_QUOTA => NativeCall::JobGetQuota,
        PROCESS_CREATE => NativeCall::ProcessCreate,
        PROCESS_START => NativeCall::ProcessStart,
        PROCESS_GIVE => NativeCall::ProcessGive,
        PROCESS_BOOTSTRAP => NativeCall::ProcessBootstrap,
        PROCESS_STATUS => NativeCall::ProcessStatus,
        INTERRUPT_CREATE => NativeCall::InterruptCreate,
        INTERRUPT_BIND => NativeCall::InterruptBind,
        INTERRUPT_ACK => NativeCall::InterruptAck,
        IO_MAPPING_CREATE => NativeCall::IoMappingCreate,
        IO_MAPPING_MAP => NativeCall::IoMappingMap,
        IO_MAPPING_MAP_COMBINING => NativeCall::IoMappingMapCombining,
        BLOCK_RING_CREATE => NativeCall::BlockRingCreate,
        NET_RING_CREATE => NativeCall::NetRingCreate,
        DISPLAY_CONTROL_CREATE => NativeCall::DisplayControlCreate,
        INPUT_CONTROL_CREATE => NativeCall::InputControlCreate,
        RENDER_CONTROL_CREATE => NativeCall::RenderControlCreate,
        DEVICE_INFO => NativeCall::DeviceInfo,
        DEVICE_QUIESCE => NativeCall::DeviceQuiesce,
        DEVICE_CLOCK => NativeCall::DeviceClock,
        SOUND_CONTROL_CREATE => NativeCall::SoundControlCreate,
        LOG_CONTROL_CREATE => NativeCall::LogControlCreate,
        DEVMGR_START => NativeCall::DevmgrStart,
        AUDIT_READ => NativeCall::AuditRead,
        DEVICE_APERTURE => NativeCall::DeviceAperture,
        DEVICE_CONFIG_READ => NativeCall::DeviceConfigRead,
        DEVICE_CONFIG_WRITE => NativeCall::DeviceConfigWrite,
        DEVICE_SET_LIMIT => NativeCall::DeviceSetLimit,
        DEVICE_GET_LIMIT => NativeCall::DeviceGetLimit,
        DEVICE_ISOLATION => NativeCall::DeviceIsolation,
        CHARDEV_CONTROL_CREATE => NativeCall::ChardevControlCreate,
        CHARDEV_REPLY => NativeCall::ChardevReply,
        CHARDEV_COPY_IN => NativeCall::ChardevCopyIn,
        CHARDEV_COPY_OUT => NativeCall::ChardevCopyOut,
        _ => return None,
    };
    Some(call)
}

/// The number a call is made with.
#[must_use]
pub const fn number(call: NativeCall) -> usize {
    match call {
        NativeCall::HandleClose => HANDLE_CLOSE,
        NativeCall::HandleDuplicate => HANDLE_DUPLICATE,
        NativeCall::HandleReplace => HANDLE_REPLACE,
        NativeCall::ObjectWaitOne => OBJECT_WAIT_ONE,
        NativeCall::ObjectWaitAsync => OBJECT_WAIT_ASYNC,
        NativeCall::ChannelCreate => CHANNEL_CREATE,
        NativeCall::ChannelWrite => CHANNEL_WRITE,
        NativeCall::ChannelRead => CHANNEL_READ,
        NativeCall::ChannelWriteRead => CHANNEL_WRITE_READ,
        NativeCall::PortCreate => PORT_CREATE,
        NativeCall::PortQueue => PORT_QUEUE,
        NativeCall::PortWait => PORT_WAIT,
        NativeCall::PortFd => PORT_FD,
        NativeCall::VmoCreate => VMO_CREATE,
        NativeCall::VmoRead => VMO_READ,
        NativeCall::VmoWrite => VMO_WRITE,
        NativeCall::VmoGetSize => VMO_GET_SIZE,
        NativeCall::VmoMap => VMO_MAP,
        NativeCall::VmoPin => VMO_PIN,
        NativeCall::VmoPinAddresses => VMO_PIN_ADDRESSES,
        NativeCall::JobCreate => JOB_CREATE,
        NativeCall::JobKill => JOB_KILL,
        NativeCall::JobForCgroup => JOB_FOR_CGROUP,
        NativeCall::JobSetLimit => JOB_SET_LIMIT,
        NativeCall::JobGetQuota => JOB_GET_QUOTA,
        NativeCall::ProcessCreate => PROCESS_CREATE,
        NativeCall::ProcessStart => PROCESS_START,
        NativeCall::ProcessGive => PROCESS_GIVE,
        NativeCall::ProcessBootstrap => PROCESS_BOOTSTRAP,
        NativeCall::ProcessStatus => PROCESS_STATUS,
        NativeCall::InterruptCreate => INTERRUPT_CREATE,
        NativeCall::InterruptBind => INTERRUPT_BIND,
        NativeCall::InterruptAck => INTERRUPT_ACK,
        NativeCall::IoMappingCreate => IO_MAPPING_CREATE,
        NativeCall::IoMappingMap => IO_MAPPING_MAP,
        NativeCall::IoMappingMapCombining => IO_MAPPING_MAP_COMBINING,
        NativeCall::BlockRingCreate => BLOCK_RING_CREATE,
        NativeCall::NetRingCreate => NET_RING_CREATE,
        NativeCall::DisplayControlCreate => DISPLAY_CONTROL_CREATE,
        NativeCall::InputControlCreate => INPUT_CONTROL_CREATE,
        NativeCall::RenderControlCreate => RENDER_CONTROL_CREATE,
        NativeCall::DeviceInfo => DEVICE_INFO,
        NativeCall::DeviceQuiesce => DEVICE_QUIESCE,
        NativeCall::DeviceClock => DEVICE_CLOCK,
        NativeCall::SoundControlCreate => SOUND_CONTROL_CREATE,
        NativeCall::LogControlCreate => LOG_CONTROL_CREATE,
        NativeCall::DevmgrStart => DEVMGR_START,
        NativeCall::AuditRead => AUDIT_READ,
        NativeCall::DeviceAperture => DEVICE_APERTURE,
        NativeCall::DeviceConfigRead => DEVICE_CONFIG_READ,
        NativeCall::DeviceConfigWrite => DEVICE_CONFIG_WRITE,
        NativeCall::DeviceSetLimit => DEVICE_SET_LIMIT,
        NativeCall::DeviceGetLimit => DEVICE_GET_LIMIT,
        NativeCall::DeviceIsolation => DEVICE_ISOLATION,
        NativeCall::ChardevControlCreate => CHARDEV_CONTROL_CREATE,
        NativeCall::ChardevReply => CHARDEV_REPLY,
        NativeCall::ChardevCopyIn => CHARDEV_COPY_IN,
        NativeCall::ChardevCopyOut => CHARDEV_COPY_OUT,
    }
}
