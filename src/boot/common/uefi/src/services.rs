//! Safe-ish wrappers over the boot services the loader uses.
//!
//! Each wrapper turns a UEFI status into a [`BootError`] carrying both what was
//! being attempted and what firmware said, because "load error" on its own at
//! this stage of boot is close to useless.

use core::cell::{Cell, RefCell};
use core::ffi::c_void;
use core::fmt;
use core::ptr;

use ferrix_bootinfo::PAGE_SIZE;

use crate::uefi::protocols::{
    BLOCK_IO_GUID, BlockIo, DEVICE_PATH_GUID, FILE_MODE_READ, FILE_POSITION_END, FileProtocol,
    GRAPHICS_OUTPUT_GUID, GraphicsOutput, IPXE_DOWNLOAD_GUID, IpxeDownload, LOADED_IMAGE_GUID,
    LoadedImage, RNG_GUID, Rng, SIMPLE_FILE_SYSTEM_GUID, SimpleFileSystem,
};
use crate::uefi::tables::{
    AllocateType, BootServices, MemoryDescriptor, MemoryType, SystemTable, Time,
};
use crate::uefi::{Guid, Handle, Status};

/// `LocateHandleBuffer`'s search by protocol.
const BY_PROTOCOL: u32 = 2;

/// How much a framebuffer is worth keeping, highest first: one a panic can
/// be drawn on and the allocator will not own; one a panic can be drawn on;
/// one the allocator will not own; the rest. Ties go to the output firmware
/// listed first.
fn rank(framebuffer: &ferrix_bootinfo::Framebuffer) -> u8 {
    let drawable =
        framebuffer.is_present() && framebuffer.format != ferrix_bootinfo::PixelFormat::Unknown;
    match (drawable, framebuffer.is_reclaimable()) {
        (true, false) => 3,
        (true, true) => 2,
        (false, false) => 1,
        (false, true) => 0,
    }
}

/// The linear framebuffer behind a graphics output, if it has one.
fn describe_output(graphics: *mut GraphicsOutput) -> Option<ferrix_bootinfo::Framebuffer> {
    // SAFETY: firmware returned a live `GraphicsOutput`.
    let output = unsafe { &*graphics };
    if output.mode.is_null() {
        return None;
    }
    // SAFETY: checked non-null just above, and firmware owns it.
    let mode = unsafe { *output.mode };
    if mode.info.is_null() {
        return None;
    }
    // SAFETY: as above, for the mode's info block.
    let info = unsafe { *mode.info };

    use crate::uefi::protocols::GraphicsPixelFormat;
    // Red, green and blue masks a bit-mask mode gives for the two byte orders
    // the kernel draws: such a mode is one of those under another name.
    const BGRX_MASKS: [u32; 3] = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff];
    const RGBX_MASKS: [u32; 3] = [0x0000_00ff, 0x0000_ff00, 0x00ff_0000];
    let [red, green, blue, reserved] = info.pixel_information;
    let format = match info.pixel_format {
        GraphicsPixelFormat::BLUE_GREEN_RED_RESERVED => ferrix_bootinfo::PixelFormat::Bgrx8888,
        GraphicsPixelFormat::RED_GREEN_BLUE_RESERVED => ferrix_bootinfo::PixelFormat::Rgbx8888,
        GraphicsPixelFormat::BIT_MASK if [red, green, blue] == BGRX_MASKS => {
            ferrix_bootinfo::PixelFormat::Bgrx8888
        }
        GraphicsPixelFormat::BIT_MASK if [red, green, blue] == RGBX_MASKS => {
            ferrix_bootinfo::PixelFormat::Rgbx8888
        }
        // Any other bit-mask or a blt-only mode is a framebuffer the kernel
        // cannot treat as an array of pixels, so report it as no framebuffer.
        _ => ferrix_bootinfo::PixelFormat::Unknown,
    };
    crate::console::println!(
        "  graphics mode {} of {}: pixel format {} (masks r {red:#x} g {green:#x} b {blue:#x} x {reserved:#x}), {} pixels a line: {}",
        mode.mode,
        mode.max_mode,
        info.pixel_format.0,
        info.pixels_per_scan_line,
        match format {
            ferrix_bootinfo::PixelFormat::Unknown => "not drawable",
            _ => "drawable",
        }
    );

    Some(ferrix_bootinfo::Framebuffer {
        phys: mode.framebuffer_base,
        size: mode.framebuffer_size as u64,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        format,
        reclaimable: 0,
        reserved: 0,
    })
}

/// Longest path the loader will open, in UCS-2 units including the terminator.
const MAX_PATH: usize = 128;

/// Something went wrong before the kernel could be started.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootError {
    /// What was being attempted.
    pub(crate) context: &'static str,
    /// What firmware said, if firmware was involved.
    pub(crate) status: Option<Status>,
}

impl BootError {
    /// An error firmware reported.
    pub(crate) const fn firmware(context: &'static str, status: Status) -> BootError {
        BootError {
            context,
            status: Some(status),
        }
    }

    /// An error the loader decided on its own.
    pub(crate) const fn plain(context: &'static str) -> BootError {
        BootError {
            context,
            status: None,
        }
    }
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (EFI_STATUS {})", self.context, status.code()),
            None => f.write_str(self.context),
        }
    }
}

/// Shorthand for the loader's fallible operations.
pub(crate) type Result<T> = core::result::Result<T, BootError>;

/// Turn a status into a result.
fn check(context: &'static str, status: Status) -> Result<()> {
    if status.is_success() {
        Ok(())
    } else {
        Err(BootError::firmware(context, status))
    }
}

/// A region of physical memory the loader allocated.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Allocation {
    /// Physical address, page aligned.
    pub(crate) address: u64,
    /// Length in bytes, a multiple of [`PAGE_SIZE`].
    pub(crate) len: u64,
}

/// The firmware memory map, as handed over.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MemoryMap {
    buffer: *const u8,
    size: usize,
    /// Firmware's stride between descriptors, which may exceed
    /// `size_of::<MemoryDescriptor>()`.
    descriptor_size: usize,
    /// The value `exit_boot_services` has to be given.
    pub(crate) key: usize,
}

impl MemoryMap {
    /// Walk the descriptors, using firmware's stride rather than ours.
    pub(crate) fn entries(&self) -> impl Iterator<Item = MemoryDescriptor> + Clone + '_ {
        // `checked_div` rather than a guard: a zero stride would mean
        // firmware reported no descriptor size, and dividing by it is the one
        // way this loop could fault.
        let count = self.size.checked_div(self.descriptor_size).unwrap_or(0);
        (0..count).map(move |index| {
            // SAFETY: `index` is below the descriptor count computed from the
            // size firmware reported, and the buffer is alive for `'_`.
            let at = unsafe { self.buffer.add(index * self.descriptor_size) };
            // SAFETY: firmware guarantees a descriptor at every multiple of
            // `descriptor_size`; it may be larger than ours but never smaller.
            unsafe { ptr::read_unaligned(at.cast::<MemoryDescriptor>()) }
        })
    }
}

/// The loader's handle on firmware.
pub(crate) struct Services {
    image: Handle,
    system_table: *mut SystemTable,
    boot: *mut BootServices,
    /// One past the highest physical address an allocation may reach, or
    /// `u64::MAX` for anywhere; see [`Services::allocate_below`].
    ceiling: Cell<u64>,
    /// The allocations of a Ferrix memory type, which firmware was asked for
    /// as loader data ([`Services::firmware_kind`]).
    tags: RefCell<Tags>,
}

/// An allocation of one of the loader's own memory types.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tag {
    /// First physical address.
    pub(crate) base: u64,
    /// Length in bytes, whole pages.
    pub(crate) len: u64,
    /// The Ferrix type it is, for the kernel's map.
    pub(crate) kind: MemoryType,
}

/// The most allocations of the loader's own types it can mark.
const MAX_TAGS: usize = 128;

/// The table of [`Tag`]s, in the order they were made.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tags {
    entries: [Tag; MAX_TAGS],
    count: usize,
}

impl Tags {
    /// The tags made and not freed.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Tag> {
        self.entries.iter().take(self.count)
    }
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Services")
    }
}

impl Services {
    /// Wrap the arguments firmware passed to `efi_main`.
    ///
    /// # Safety
    ///
    /// `image` and `system_table` must be the values firmware passed, and boot
    /// services must not have been exited.
    pub(crate) unsafe fn new(image: Handle, system_table: *mut SystemTable) -> Result<Services> {
        if system_table.is_null() {
            return Err(BootError::plain("firmware passed a null system table"));
        }
        // SAFETY: the caller guarantees this is firmware's own system table.
        let boot = unsafe { (*system_table).boot_services };
        if boot.is_null() {
            return Err(BootError::plain("system table has no boot services"));
        }
        Ok(Services {
            image,
            system_table,
            boot,
            ceiling: Cell::new(u64::MAX),
            tags: RefCell::new(Tags {
                entries: [Tag {
                    base: 0,
                    len: 0,
                    kind: MemoryType::LOADER_DATA,
                }; MAX_TAGS],
                count: 0,
            }),
        })
    }

    /// The system table, for the fields the kernel is handed directly.
    pub(crate) const fn system_table(&self) -> *mut SystemTable {
        self.system_table
    }

    /// Borrow firmware's boot services table.
    ///
    /// Every call below goes through this rather than dereferencing
    /// `self.boot` inline. That is not a style choice: a UEFI call is a
    /// dereference *and* an indirect call through a function pointer, which is
    /// two unsafe operations, and the workspace requires one claim per block.
    /// Naming the dereference once here leaves each call site with exactly one
    /// thing to justify -- that it is calling the right service with arguments
    /// firmware may write through.
    fn boot(&self) -> &BootServices {
        // SAFETY: checked non-null in `new`, and firmware guarantees the table
        // is live until `exit_boot_services`, after which nothing in this
        // module is called again.
        unsafe { &*self.boot }
    }

    /// Borrow firmware's system table.
    fn table(&self) -> &SystemTable {
        // SAFETY: checked non-null in `new`; firmware owns it and it outlives
        // the loader.
        unsafe { &*self.system_table }
    }

    // -- memory ------------------------------------------------------------

    /// Keep every later allocation below `end`.
    ///
    /// Firmware allocates from the top of RAM down, so on a 32-bit machine with
    /// more RAM than the direct map holds, the kernel image and everything
    /// handed over with it would otherwise land where the kernel cannot reach
    /// them. An allocation that does not fit below `end` fails rather than
    /// going above it.
    pub(crate) fn allocate_below(&self, end: u64) {
        self.ceiling.set(end);
    }

    /// Allocate `len` bytes of physical memory, rounded up to whole pages.
    ///
    /// The `kind` is what firmware's memory map will report the region as, and
    /// the loader uses its own reserved types so the kernel learns what each
    /// allocation is for from the map itself.
    pub(crate) fn allocate(
        &self,
        context: &'static str,
        len: u64,
        kind: MemoryType,
    ) -> Result<Allocation> {
        self.allocate_under(context, len, kind, u64::MAX)
    }

    /// [`Services::allocate`], with every byte also below `end`.
    pub(crate) fn allocate_under(
        &self,
        context: &'static str,
        len: u64,
        kind: MemoryType,
        end: u64,
    ) -> Result<Allocation> {
        let pages = len.div_ceil(PAGE_SIZE);
        if pages == 0 {
            return Err(BootError::plain(context));
        }
        // For MAX_ADDRESS the address going in is the highest byte allowed.
        let (how, mut address) = match self.ceiling.get().min(end) {
            u64::MAX => (AllocateType::ANY_PAGES, 0u64),
            end => (AllocateType::MAX_ADDRESS, end - 1),
        };
        // SAFETY: `address` is a live local, and `pages` is non-zero.
        let status = unsafe {
            (self.boot().allocate_pages)(
                how,
                Self::firmware_kind(kind),
                pages as usize,
                &raw mut address,
            )
        };
        check(context, status)?;

        let allocation = Allocation {
            address,
            len: pages * PAGE_SIZE,
        };
        if !self.tag(allocation, kind) {
            let _ = self.free(context, allocation);
            return Err(BootError::plain(
                "more allocations of the loader's own types than it can mark",
            ));
        }
        self.zero(allocation);
        Ok(allocation)
    }

    /// Allocate `len` bytes at exactly `address`, rounded up to whole pages,
    /// and leave what is there as it is: `None` when firmware will not, said
    /// by the caller. For the pstore region, whose bytes are what the
    /// previous kernel left.
    pub(crate) fn allocate_at(
        &self,
        address: u64,
        len: u64,
        kind: MemoryType,
    ) -> Option<Allocation> {
        let pages = len.div_ceil(PAGE_SIZE);
        let end = address.checked_add(pages.checked_mul(PAGE_SIZE)?)?;
        if pages == 0 || !address.is_multiple_of(PAGE_SIZE) || end > self.ceiling.get() {
            return None;
        }
        let mut at = address;
        // SAFETY: `at` is a live local, and `pages` is non-zero.
        let status = unsafe {
            (self.boot().allocate_pages)(
                AllocateType::ADDRESS,
                Self::firmware_kind(kind),
                pages as usize,
                &raw mut at,
            )
        };
        let allocation = (status.is_success() && at == address).then_some(Allocation {
            address,
            len: pages * PAGE_SIZE,
        })?;
        if !self.tag(allocation, kind) {
            let _ = self.free("freeing an allocation it could not mark", allocation);
            return None;
        }
        Some(allocation)
    }

    /// What firmware is asked for when the loader wants `kind`: loader data
    /// for every type of the loader's own.
    ///
    /// The N150's AMI firmware never returned from `exit_boot_services` once
    /// memory of an OS-defined type (`0x8000_0000` and up) existed, which the
    /// specification allows and OVMF accepts. So firmware only ever sees
    /// loader data, and the kernel's map gets the loader's types from
    /// [`Services::tags`] (`ferrix_bootinfo::retag`).
    const fn firmware_kind(kind: MemoryType) -> MemoryType {
        if kind.0 >= 0x8000_0000 {
            MemoryType::LOADER_DATA
        } else {
            kind
        }
    }

    /// Remember `allocation` as `kind` when that is one of the loader's own
    /// types; false when the table is full.
    fn tag(&self, allocation: Allocation, kind: MemoryType) -> bool {
        if kind.0 < 0x8000_0000 {
            return true;
        }
        let mut tags = self.tags.borrow_mut();
        let count = tags.count;
        let Some(slot) = tags.entries.get_mut(count) else {
            return false;
        };
        *slot = Tag {
            base: allocation.address,
            len: allocation.len,
            kind,
        };
        tags.count = count + 1;
        true
    }

    /// The allocations of the loader's own types still held.
    pub(crate) fn tags(&self) -> Tags {
        *self.tags.borrow()
    }

    /// Give an allocation back to firmware.
    pub(crate) fn free(&self, context: &'static str, allocation: Allocation) -> Result<()> {
        {
            let mut tags = self.tags.borrow_mut();
            let count = tags.count;
            if let Some(at) = tags
                .entries
                .iter()
                .take(count)
                .position(|tag| tag.base == allocation.address)
            {
                tags.entries.copy_within(at + 1..count, at);
                tags.count = count - 1;
            }
        }
        // SAFETY: the range came from `allocate`, which rounded it to whole
        // pages, and the caller holds nothing inside it any more.
        let status = unsafe {
            (self.boot().free_pages)(allocation.address, (allocation.len / PAGE_SIZE) as usize)
        };
        check(context, status)
    }

    /// Fill an allocation with zeroes.
    ///
    /// Firmware does not promise fresh pages are clean, and a page table built
    /// on top of someone else's leftovers translates to wherever they pointed.
    fn zero(&self, allocation: Allocation) {
        // SAFETY: the range was just allocated to us, is identity mapped under
        // boot services, and nothing else refers to it.
        unsafe {
            ptr::write_bytes(allocation.address as *mut u8, 0, allocation.len as usize);
        }
    }

    /// Fetch the memory map into a buffer the loader already owns.
    ///
    /// The buffer has to be allocated in advance: asking firmware for memory is
    /// itself a change to the map, so a map fetched into a fresh allocation is
    /// stale before it is read.
    pub(crate) fn memory_map(&self, buffer: Allocation) -> Result<MemoryMap> {
        let mut size = buffer.len as usize;
        let mut key = 0usize;
        let mut descriptor_size = 0usize;
        let mut version = 0u32;

        // SAFETY: every out-parameter is a live local, and the buffer is ours.
        let status = unsafe {
            (self.boot().get_memory_map)(
                &raw mut size,
                buffer.address as *mut MemoryDescriptor,
                &raw mut key,
                &raw mut descriptor_size,
                &raw mut version,
            )
        };
        check("get_memory_map", status)?;

        Ok(MemoryMap {
            buffer: buffer.address as *const u8,
            size,
            descriptor_size,
            key,
        })
    }

    /// How many bytes the memory map currently needs, with slack.
    ///
    /// The slack matters: allocating the buffer adds entries to the map, so a
    /// buffer sized to today's map is too small by the time it is used.
    pub(crate) fn memory_map_size(&self) -> Result<u64> {
        let mut size = 0usize;
        let mut key = 0usize;
        let mut descriptor_size = 0usize;
        let mut version = 0u32;

        // SAFETY: a zero size asks firmware for the size it needs, which it
        // reports by returning BUFFER_TOO_SMALL and writing `size`.
        let status = unsafe {
            (self.boot().get_memory_map)(
                &raw mut size,
                ptr::null_mut(),
                &raw mut key,
                &raw mut descriptor_size,
                &raw mut version,
            )
        };
        if status != Status::BUFFER_TOO_SMALL && status.is_error() {
            return Err(BootError::firmware("sizing the memory map", status));
        }
        Ok(size as u64 + 8 * descriptor_size as u64)
    }

    // -- files -------------------------------------------------------------

    /// Read a file from the volume the loader was itself loaded from.
    ///
    /// Returns the allocation holding it and the file's real length, which is
    /// smaller than the allocation whenever the file is not a whole number of
    /// pages.
    pub(crate) fn read_file(&self, path: &str, kind: MemoryType) -> Result<(Allocation, u64)> {
        let root = self.open_volume()?;
        let file = match self.open_file(root, path) {
            Ok(file) => file,
            Err(error) => {
                // A missing file is an answer a caller may expect -- the
                // initramfs is optional -- so the volume handle must not
                // outlive it.
                //
                // SAFETY: firmware returned `root` from `open_volume`, so it is
                // a live `FileProtocol`.
                let volume = unsafe { &*root };
                // SAFETY: `root` is open and is closed exactly once, here.
                let _ = unsafe { (volume.close)(root) };
                return Err(error);
            }
        };

        let size = self.file_size(file)?;
        let allocation = self.allocate("allocating for a file", size.max(1), kind)?;

        // SAFETY: firmware returned this handle from `open`, so it is a live
        // `FileProtocol` until it is closed below.
        let handle = unsafe { &*file };

        let mut remaining = size as usize;
        let mut at = allocation.address as *mut u8;
        while remaining > 0 {
            let mut chunk = remaining;
            // SAFETY: `at` is inside our own allocation and `chunk` is what is
            // left of the file, so firmware writes only where it may.
            let status = unsafe { (handle.read)(file, &raw mut chunk, at) };
            check("reading a file", status)?;
            if chunk == 0 {
                return Err(BootError::plain("file ended before its stated length"));
            }
            remaining -= chunk;
            // SAFETY: `chunk` bytes were just read into the allocation, so
            // advancing by it stays inside it.
            at = unsafe { at.add(chunk) };
        }

        // SAFETY: `file` is open and is closed exactly once, here.
        let _ = unsafe { (handle.close)(file) };

        // SAFETY: firmware returned `root` from `open_volume`, so it is a live
        // `FileProtocol`.
        let volume = unsafe { &*root };
        // SAFETY: `root` is open and is closed exactly once, here.
        let _ = unsafe { (volume.close)(root) };

        Ok((allocation, size))
    }

    /// Where firmware loaded this program, and how many bytes of it there are.
    ///
    /// The switch to the loader's own tables fetches its next instruction at
    /// the address it was fetching from before them, so the loader's own code
    /// has to be mapped at its own address across it. On a machine whose RAM
    /// is above the split that mapping is these pages rather than all of RAM,
    /// so the loader has to know which pages are its own.
    pub(crate) fn image_range(&self) -> Result<(u64, u64)> {
        let image: *mut LoadedImage =
            self.protocol(self.image, &LOADED_IMAGE_GUID, "loaded image")?;
        // SAFETY: firmware returned a live `LoadedImage` for our own handle.
        let loaded = unsafe { &*image };
        Ok((loaded.image_base.addr() as u64, loaded.image_size))
    }

    /// Open the root directory of the loader's own volume.
    fn open_volume(&self) -> Result<*mut FileProtocol> {
        let image: *mut LoadedImage =
            self.protocol(self.image, &LOADED_IMAGE_GUID, "loaded image")?;
        // SAFETY: firmware returned a live `LoadedImage` for our own handle.
        let loaded = unsafe { &*image };
        let device = loaded.device_handle;

        let filesystem: *mut SimpleFileSystem =
            self.protocol(device, &SIMPLE_FILE_SYSTEM_GUID, "simple file system")?;
        // SAFETY: firmware returned a live `SimpleFileSystem` for that handle.
        let volume = unsafe { &*filesystem };

        let mut root = ptr::null_mut();
        // SAFETY: `root` is a live local for firmware to write the handle into.
        let status = unsafe { (volume.open_volume)(filesystem, &raw mut root) };
        check("opening the boot volume", status)?;
        Ok(root)
    }

    /// Open one file under `root`.
    fn open_file(&self, root: *mut FileProtocol, path: &str) -> Result<*mut FileProtocol> {
        let mut wide = [0u16; MAX_PATH];
        widen(path, &mut wide)?;

        let mut file = ptr::null_mut();
        // SAFETY: `root` is an open directory handle from `open_volume`.
        let directory = unsafe { &*root };
        // SAFETY: `wide` is NUL terminated by `widen`, and `file` is a live
        // local for firmware to write the new handle into.
        let status =
            unsafe { (directory.open)(root, &raw mut file, wide.as_ptr(), FILE_MODE_READ, 0) };
        check("opening a file", status)?;
        Ok(file)
    }

    /// The length of an open file.
    ///
    /// Found by seeking to the end and asking where that was, which avoids the
    /// `FileInfo` structure and its trailing variable-length name.
    fn file_size(&self, file: *mut FileProtocol) -> Result<u64> {
        // SAFETY: firmware returned `file` from `open`, so it is live.
        let handle = unsafe { &*file };

        // SAFETY: seeking takes only the handle and a position.
        let status = unsafe { (handle.set_position)(file, FILE_POSITION_END) };
        check("seeking to the end of a file", status)?;

        let mut size = 0u64;
        // SAFETY: `size` is a live local for firmware to write into.
        let status = unsafe { (handle.get_position)(file, &raw mut size) };
        check("reading a file position", status)?;

        // SAFETY: as the first call.
        let status = unsafe { (handle.set_position)(file, 0) };
        check("rewinding a file", status)?;

        Ok(size)
    }

    // -- the boot disk -----------------------------------------------------

    /// The whole disk the loader's own partition is on, when firmware
    /// loaded it from a partition (a `HardDrive` node ends its volume's
    /// device path); `None` for a volume that is a whole disk -- today's
    /// bare FAT image -- which has no slots.
    pub(crate) fn boot_disk(&self) -> Result<Option<Disk>> {
        let image: *mut LoadedImage =
            self.protocol(self.image, &LOADED_IMAGE_GUID, "loaded image")?;
        // SAFETY: firmware returned a live `LoadedImage` for our own handle.
        let device = unsafe { &*image }.device_handle;
        let Ok(path) = self.protocol::<u8>(device, &DEVICE_PATH_GUID, "device path") else {
            return Ok(None);
        };
        let Some(prefix) = disk_prefix(path) else {
            return Ok(None);
        };
        // The disk's path: the volume's up to its HardDrive node, then an
        // end node, in a copy of our own.
        let mut copy = [0u8; MAX_DEVICE_PATH + 4];
        let Some(head) = copy.get_mut(..prefix) else {
            return Ok(None);
        };
        // SAFETY: `disk_prefix` walked these `prefix` bytes of firmware's
        // device path node by node, so they are readable.
        head.copy_from_slice(unsafe { core::slice::from_raw_parts(path.cast_const(), prefix) });
        if let Some(end) = copy.get_mut(prefix..prefix + 4) {
            end.copy_from_slice(&END_NODE);
        }
        let mut cursor: *const u8 = copy.as_ptr();
        let mut handle: Handle = ptr::null_mut();
        // SAFETY: `copy` is a terminated device path that outlives the call,
        // and `cursor` and `handle` are live locals firmware writes.
        let status = unsafe {
            (self.boot().locate_device_path)(&BLOCK_IO_GUID, &raw mut cursor, &raw mut handle)
        };
        if !status.is_success() || handle.is_null() {
            return Ok(None);
        }
        // Only the whole disk itself: the handle's path must be all of the
        // prefix, so that what remains is the end node.
        // SAFETY: firmware advanced `cursor` within `copy`.
        let rest = unsafe { core::slice::from_raw_parts(cursor, 4) };
        if rest != END_NODE {
            return Ok(None);
        }
        let block_io: *mut BlockIo = self.protocol(handle, &BLOCK_IO_GUID, "block I/O")?;
        // SAFETY: firmware returned a live `BlockIo`.
        let media = unsafe { (*block_io).media };
        if media.is_null() {
            return Ok(None);
        }
        // SAFETY: checked non-null; firmware owns it for the life of the handle.
        let media = unsafe { *media };
        if media.logical_partition != 0 || media.media_present == 0 {
            return Ok(None);
        }
        let buffer = self.allocate(
            "allocating the boot disk's block buffer",
            PAGE_SIZE.max(u64::from(media.block_size)),
            MemoryType::LOADER_DATA,
        )?;
        Ok(Some(Disk {
            block_io,
            media_id: media.media_id,
            block_size: media.block_size,
            last_block: media.last_block,
            buffer,
        }))
    }

    // -- protocols and tables ---------------------------------------------

    /// Fetch a protocol from a handle.
    fn protocol<T>(&self, handle: Handle, guid: &Guid, context: &'static str) -> Result<*mut T> {
        let mut interface: *mut c_void = ptr::null_mut();
        // SAFETY: `handle` came from firmware, `guid` is a live local constant,
        // and `interface` is a live local.
        let status = unsafe { (self.boot().handle_protocol)(handle, guid, &raw mut interface) };
        check(context, status)?;
        if interface.is_null() {
            return Err(BootError::plain(context));
        }
        Ok(interface.cast::<T>())
    }

    /// The framebuffer to hand the kernel, and how many graphics outputs
    /// firmware offered.
    ///
    /// Every graphics output is looked at, not just the first firmware
    /// returns, because firmware can offer more than one and the first is not
    /// always the one to keep. On QEMU's AArch64 machine with a virtio-gpu,
    /// AAVMF's `VirtioGpuDxe` offers one whose framebuffer is boot-services
    /// data, which the kernel's frame allocator hands out again, beside
    /// `ramfb`'s, which is reserved. [`rank`] prefers one a panic can be drawn
    /// on and whose framebuffer the allocator will not own;
    /// [`ferrix_bootinfo::allocator_owns`] decides the second over `map`, and
    /// the choice's `reclaimable` says what it found.
    ///
    /// A machine with no graphics output is not an error: the serial console is
    /// the one the boot test reads.
    pub(crate) fn framebuffer(
        &self,
        map: &MemoryMap,
    ) -> (Option<ferrix_bootinfo::Framebuffer>, usize) {
        let mut count = 0usize;
        let mut handles: *mut Handle = ptr::null_mut();
        // SAFETY: all out-parameters are live locals; the GUID is a constant.
        let status = unsafe {
            (self.boot().locate_handle_buffer)(
                BY_PROTOCOL,
                &GRAPHICS_OUTPUT_GUID,
                ptr::null_mut(),
                &raw mut count,
                &raw mut handles,
            )
        };
        if !status.is_success() || handles.is_null() {
            return (None, 0);
        }

        let mut chosen: Option<ferrix_bootinfo::Framebuffer> = None;
        let mut outputs = 0;
        for index in 0..count {
            // SAFETY: `index` is below the count firmware wrote with the
            // buffer, so the offset stays inside it.
            let slot = unsafe { handles.add(index) };
            // SAFETY: firmware filled every one of those `count` slots.
            let handle = unsafe { *slot };
            let Ok(graphics) =
                self.protocol::<GraphicsOutput>(handle, &GRAPHICS_OUTPUT_GUID, "graphics output")
            else {
                continue;
            };
            let Some(mut candidate) = describe_output(graphics) else {
                continue;
            };
            outputs += 1;
            let owned = ferrix_bootinfo::allocator_owns(
                map.entries()
                    .map(|descriptor| crate::load::describe(&descriptor)),
                candidate.phys,
                candidate.size,
            );
            candidate.reclaimable = u32::from(owned);
            if chosen.is_none_or(|current| rank(&candidate) > rank(&current)) {
                chosen = Some(candidate);
            }
        }
        // SAFETY: firmware allocated `handles` from pool for us to free.
        let _ = unsafe { (self.boot().free_pool)(handles.cast::<u8>()) };
        (chosen, outputs)
    }

    /// The time of day from firmware's real-time clock, as nanoseconds since
    /// the Unix epoch, or `None` when firmware has no clock or reports a time
    /// that is not a date.
    pub(crate) fn firmware_time(&self) -> Option<i64> {
        let runtime = self.table().runtime_services;
        if runtime.is_null() {
            return None;
        }
        let mut time = Time::default();
        // SAFETY: firmware owns the runtime services table, checked non-null.
        let get_time = unsafe { (*runtime).get_time };
        // SAFETY: `get_time` writes one `EFI_TIME` into a live local; a null
        // capabilities pointer is allowed.
        let status = unsafe { get_time(&raw mut time, ptr::null_mut()) };
        if status.is_error() {
            return None;
        }
        let offset = if time.time_zone == Time::UNSPECIFIED_TIMEZONE {
            0
        } else {
            // EFI_TIME counts minutes *west* of UTC as positive: local time
            // is UTC less the zone.
            time.time_zone.checked_neg()?
        };
        ferrix_bootinfo::unix_nanos(
            time.year,
            time.month,
            time.day,
            time.hour,
            time.minute,
            time.second,
            time.nanosecond,
            offset,
        )
    }

    /// Thirty-two bytes from firmware's `EFI_RNG_PROTOCOL`, or `None` when
    /// firmware offers none or it fails.
    pub(crate) fn firmware_seed(&self) -> Option<[u8; 32]> {
        self.firmware_random()
    }

    /// `N` bytes from firmware's `EFI_RNG_PROTOCOL`, or `None` when firmware
    /// offers none or it fails. Each call asks again, so two callers never
    /// share bytes.
    pub(crate) fn firmware_random<const N: usize>(&self) -> Option<[u8; N]> {
        let mut interface: *mut c_void = ptr::null_mut();
        // SAFETY: `locate_protocol` writes an interface pointer into a live
        // local; boot services are live.
        let status = unsafe {
            (self.boot().locate_protocol)(&RNG_GUID, ptr::null_mut(), &raw mut interface)
        };
        if status.is_error() || interface.is_null() {
            return None;
        }
        let rng = interface.cast::<Rng>();
        let mut seed = [0_u8; N];
        // SAFETY: firmware returned a live `EFI_RNG_PROTOCOL`.
        let get_rng = unsafe { (*rng).get_rng };
        // SAFETY: `get_rng` writes `seed.len()` bytes into a live local, with
        // firmware's default algorithm.
        let status = unsafe { get_rng(rng, ptr::null(), seed.len(), seed.as_mut_ptr()) };
        (!status.is_error()).then_some(seed)
    }

    /// Look up a configuration table by GUID.
    pub(crate) fn configuration_table(&self, guid: &Guid) -> Option<u64> {
        let count = self.table().number_of_table_entries;
        let entries = self.table().configuration_table;
        if entries.is_null() {
            return None;
        }

        for index in 0..count {
            // SAFETY: `index` is below the count firmware reported, so this
            // stays inside the configuration table.
            let entry = unsafe { entries.add(index) };
            // SAFETY: firmware initialised every entry it counted.
            let entry = unsafe { *entry };
            if entry.vendor_guid == *guid {
                return Some(entry.vendor_table as u64);
            }
        }
        None
    }

    // -- the network, when iPXE started the loader --------------------------

    /// iPXE's download protocol, when iPXE started the loader: it is on the
    /// device handle of every program iPXE starts, and on nothing else.
    pub(crate) fn ipxe_download(&self) -> Option<*mut IpxeDownload> {
        let image: *mut LoadedImage = self
            .protocol(self.image, &LOADED_IMAGE_GUID, "loaded image")
            .ok()?;
        // SAFETY: firmware returned a live `LoadedImage` for our own handle.
        let device = unsafe { &*image }.device_handle;
        self.protocol(device, &IPXE_DOWNLOAD_GUID, "iPXE download protocol")
            .ok()
    }

    /// Wait `microseconds`, with firmware's own timer.
    pub(crate) fn stall(&self, microseconds: usize) {
        // SAFETY: `stall` takes only a count; boot services are live.
        let _ = unsafe { (self.boot().stall)(microseconds) };
    }

    // -- the last call -----------------------------------------------------

    /// Leave boot services.
    ///
    /// After this returns, nothing in this module may be called again: the
    /// function pointers are still there, and the code behind them is not.
    pub(crate) fn exit_boot_services(&self, key: usize) -> Result<()> {
        // SAFETY: `key` came from the memory map fetched immediately before.
        let status = unsafe { (self.boot().exit_boot_services)(self.image, key) };
        check("exit_boot_services", status)
    }
}

/// The longest device path the loader walks.
const MAX_DEVICE_PATH: usize = 1024;

/// A device path's end node.
const END_NODE: [u8; 4] = [0x7F, 0xFF, 4, 0];

/// The bytes of a device path before its `HardDrive` node (media type 4,
/// subtype 1), when that node is the last before the end; `None` otherwise,
/// or for a path longer than [`MAX_DEVICE_PATH`] or with a malformed node.
fn disk_prefix(path: *mut u8) -> Option<usize> {
    let mut at = 0usize;
    let mut hard_drive = None;
    while at + 4 <= MAX_DEVICE_PATH {
        // SAFETY: firmware's device path is a sequence of nodes ending in an
        // end node, each at least four bytes and saying its own length; `at`
        // is the start of a node this walk reached by those lengths.
        let node = unsafe { core::slice::from_raw_parts(path.wrapping_add(at).cast_const(), 4) };
        let [kind, subtype, low, high] = *<&[u8; 4]>::try_from(node).ok()?;
        let length = usize::from(u16::from_le_bytes([low, high]));
        if kind == 0x7F && subtype == 0xFF {
            return hard_drive.filter(|&start: &usize| start > 0);
        }
        if length < 4 {
            return None;
        }
        hard_drive = (kind == 4 && subtype == 1).then_some(at);
        at += length;
    }
    None
}

/// The whole disk the loader was started from, through Block I/O.
#[derive(Debug)]
pub(crate) struct Disk {
    block_io: *mut BlockIo,
    media_id: u32,
    pub(crate) block_size: u32,
    pub(crate) last_block: u64,
    /// A page-aligned buffer of at least one block, which satisfies any
    /// alignment firmware asks of Block I/O buffers up to a page.
    buffer: Allocation,
}

impl Disk {
    /// Blocks on the disk.
    pub(crate) const fn blocks(&self) -> u64 {
        self.last_block.saturating_add(1)
    }

    /// Read `into.len()` bytes, a whole number of blocks, from `lba` into
    /// `into`, which must be page aligned when longer than one block.
    pub(crate) fn read(&self, lba: u64, into: &mut [u8]) -> Result<()> {
        // SAFETY: firmware's live protocol for this disk, and `into` is a
        // live buffer of the length passed.
        let status = unsafe {
            (self.io().read_blocks)(
                self.block_io,
                self.media_id,
                lba,
                into.len(),
                into.as_mut_ptr(),
            )
        };
        check("reading the boot disk", status)
    }

    /// Firmware's protocol.
    fn io(&self) -> &BlockIo {
        // SAFETY: `boot_disk` checked the protocol pointer firmware returned,
        // and it lives until boot services end, after which no disk is used.
        unsafe { &*self.block_io }
    }

    /// The aligned one-block buffer, if `len` is one block.
    fn bounce(
        &self,
        len: usize,
    ) -> core::result::Result<*mut u8, ferrix_slots::record::DeviceError> {
        if len == self.block_size as usize {
            Ok(self.buffer.address as *mut u8)
        } else {
            Err(ferrix_slots::record::DeviceError)
        }
    }
}

impl ferrix_slots::record::ControlDisk for Disk {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn write_block(
        &mut self,
        lba: u64,
        bytes: &[u8],
    ) -> core::result::Result<(), ferrix_slots::record::DeviceError> {
        let bounce = self.bounce(bytes.len())?;
        // SAFETY: the bounce buffer is the loader's own allocation of at
        // least one block, and `bytes` is one block that cannot overlap it.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), bounce, bytes.len()) };
        // SAFETY: firmware's live protocol for this disk, and the bounce
        // buffer is one block, page aligned.
        let status = unsafe {
            (self.io().write_blocks)(self.block_io, self.media_id, lba, bytes.len(), bounce)
        };
        status
            .is_success()
            .then_some(())
            .ok_or(ferrix_slots::record::DeviceError)
    }

    fn flush(&mut self) -> core::result::Result<(), ferrix_slots::record::DeviceError> {
        // SAFETY: `block_io` is firmware's live protocol for this disk.
        let status = unsafe { (self.io().flush_blocks)(self.block_io) };
        status
            .is_success()
            .then_some(())
            .ok_or(ferrix_slots::record::DeviceError)
    }

    fn read_block(
        &mut self,
        lba: u64,
        bytes: &mut [u8],
    ) -> core::result::Result<(), ferrix_slots::record::DeviceError> {
        let bounce = self.bounce(bytes.len())?;
        // SAFETY: as `write_block`.
        let status = unsafe {
            (self.io().read_blocks)(self.block_io, self.media_id, lba, bytes.len(), bounce)
        };
        if !status.is_success() {
            return Err(ferrix_slots::record::DeviceError);
        }
        // SAFETY: firmware filled one block of the bounce buffer, and `bytes`
        // is one block that cannot overlap it.
        unsafe { ptr::copy_nonoverlapping(bounce.cast_const(), bytes.as_mut_ptr(), bytes.len()) };
        Ok(())
    }
}

/// Convert an ASCII path to NUL-terminated UCS-2, with UEFI's separators.
fn widen(path: &str, out: &mut [u16; MAX_PATH]) -> Result<()> {
    let mut used = 0;
    for byte in path.bytes() {
        if !byte.is_ascii() {
            return Err(BootError::plain("a boot path must be ASCII"));
        }
        // UEFI paths are backslash separated. Accepting both means the rest of
        // the loader can write ordinary paths.
        let byte = if byte == b'/' { b'\\' } else { byte };
        let Some(slot) = out.get_mut(used) else {
            return Err(BootError::plain("boot path is too long"));
        };
        *slot = u16::from(byte);
        used += 1;
    }
    let Some(slot) = out.get_mut(used) else {
        return Err(BootError::plain("boot path is too long"));
    };
    *slot = 0;
    Ok(())
}
