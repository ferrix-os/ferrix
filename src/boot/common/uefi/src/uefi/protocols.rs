//! The seven UEFI protocols the loader uses.
//!
//! As in `tables.rs`, every function pointer of every protocol is declared in
//! specification order whether or not it is called.

use core::ffi::c_void;

use super::{Guid, Status};

// ---------------------------------------------------------------------------
// Simple text output — the loader's console
// ---------------------------------------------------------------------------

/// Console output. Under QEMU with `-display none` firmware routes this to the
/// serial port, which is how the loader's messages reach the boot test.
#[repr(C)]
pub(crate) struct SimpleTextOutput {
    reset: usize,
    /// Writes a NUL-terminated UCS-2 string.
    pub(crate) output_string:
        unsafe extern "efiapi" fn(this: *mut SimpleTextOutput, string: *const u16) -> Status,
    test_string: usize,
    query_mode: usize,
    set_mode: usize,
    set_attribute: usize,
    pub(crate) clear_screen: unsafe extern "efiapi" fn(this: *mut SimpleTextOutput) -> Status,
    set_cursor_position: usize,
    enable_cursor: usize,
    mode: *mut c_void,
}

// ---------------------------------------------------------------------------
// Random numbers — a seed for the kernel's generator
// ---------------------------------------------------------------------------

/// Identifies [`Rng`].
pub(crate) const RNG_GUID: Guid = Guid::new(
    0x3152_bca5,
    0xeade,
    0x433d,
    [0x86, 0x2e, 0xc0, 0x1c, 0xdc, 0x29, 0x1f, 0x44],
);

/// `EFI_RNG_PROTOCOL`. OVMF provides it over `RDRAND`, and over a virtio-rng
/// device when there is one; not every firmware does.
#[repr(C)]
pub(crate) struct Rng {
    get_info: usize,
    /// Fills `value_length` bytes at `value`. A null algorithm is firmware's
    /// default.
    pub(crate) get_rng: unsafe extern "efiapi" fn(
        this: *mut Rng,
        algorithm: *const Guid,
        value_length: usize,
        value: *mut u8,
    ) -> Status,
}

// ---------------------------------------------------------------------------
// Loaded image — how the loader finds the volume it was loaded from
// ---------------------------------------------------------------------------

/// Identifies [`LoadedImage`].
pub(crate) const LOADED_IMAGE_GUID: Guid = Guid::new(
    0x5b1b_31a1,
    0x9562,
    0x11d2,
    [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);

/// Describes the running image. Its `device_handle` is the volume the loader
/// came from, and therefore the volume the kernel is next to.
#[repr(C)]
pub(crate) struct LoadedImage {
    pub(crate) revision: u32,
    pub(crate) parent_handle: *mut c_void,
    pub(crate) system_table: *mut c_void,
    pub(crate) device_handle: *mut c_void,
    pub(crate) file_path: *mut c_void,
    pub(crate) reserved: *mut c_void,
    pub(crate) load_options_size: u32,
    pub(crate) load_options: *mut c_void,
    pub(crate) image_base: *mut c_void,
    pub(crate) image_size: u64,
    pub(crate) image_code_type: u32,
    pub(crate) image_data_type: u32,
    unload: usize,
}

// ---------------------------------------------------------------------------
// Simple file system — reading the kernel off the ESP
// ---------------------------------------------------------------------------

/// Identifies [`SimpleFileSystem`].
pub(crate) const SIMPLE_FILE_SYSTEM_GUID: Guid = Guid::new(
    0x964e_5b22,
    0x6459,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);

/// A mounted FAT volume.
#[repr(C)]
pub(crate) struct SimpleFileSystem {
    pub(crate) revision: u64,
    pub(crate) open_volume: unsafe extern "efiapi" fn(
        this: *mut SimpleFileSystem,
        root: *mut *mut FileProtocol,
    ) -> Status,
}

/// Open a file for reading.
pub(crate) const FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;

/// A file or directory.
#[repr(C)]
pub(crate) struct FileProtocol {
    pub(crate) revision: u64,
    pub(crate) open: unsafe extern "efiapi" fn(
        this: *mut FileProtocol,
        new_handle: *mut *mut FileProtocol,
        file_name: *const u16,
        open_mode: u64,
        attributes: u64,
    ) -> Status,
    pub(crate) close: unsafe extern "efiapi" fn(this: *mut FileProtocol) -> Status,
    delete: usize,
    pub(crate) read: unsafe extern "efiapi" fn(
        this: *mut FileProtocol,
        buffer_size: *mut usize,
        buffer: *mut u8,
    ) -> Status,
    write: usize,
    pub(crate) get_position:
        unsafe extern "efiapi" fn(this: *mut FileProtocol, position: *mut u64) -> Status,
    pub(crate) set_position:
        unsafe extern "efiapi" fn(this: *mut FileProtocol, position: u64) -> Status,
    get_info: usize,
    set_info: usize,
    flush: usize,
}

/// Seeking here and asking where you are is how the file's size is obtained
/// without a `FileInfo` structure, whose trailing variable-length name makes it
/// awkward to declare and easy to get wrong.
pub(crate) const FILE_POSITION_END: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Graphics output — the framebuffer, if there is one
// ---------------------------------------------------------------------------

/// Identifies [`GraphicsOutput`].
pub(crate) const GRAPHICS_OUTPUT_GUID: Guid = Guid::new(
    0x9042_a9de,
    0x23dc,
    0x4a38,
    [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a],
);

/// How pixels are laid out in the framebuffer.
///
/// A plain integer rather than an `enum`, deliberately: this value is written
/// by firmware and read by us, and a `repr(u32)` enum holding a number we did
/// not enumerate is undefined behaviour rather than an unknown format. The same
/// argument does not apply to the types we only ever *send* to firmware.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct GraphicsPixelFormat(pub(crate) u32);

impl GraphicsPixelFormat {
    /// Red, green, blue, reserved — one byte each.
    pub(crate) const RED_GREEN_BLUE_RESERVED: GraphicsPixelFormat = GraphicsPixelFormat(0);
    /// Blue, green, red, reserved — one byte each. What QEMU reports.
    pub(crate) const BLUE_GREEN_RED_RESERVED: GraphicsPixelFormat = GraphicsPixelFormat(1);
}

/// Description of one graphics mode.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct GraphicsModeInfo {
    pub(crate) version: u32,
    pub(crate) horizontal_resolution: u32,
    pub(crate) vertical_resolution: u32,
    pub(crate) pixel_format: GraphicsPixelFormat,
    pub(crate) pixel_information: [u32; 4],
    /// Pixels, not bytes, from one scanline to the next. It can exceed the
    /// horizontal resolution, and assuming otherwise produces a picture that
    /// shears progressively down the screen.
    pub(crate) pixels_per_scan_line: u32,
}

/// The currently selected mode and where its framebuffer is.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct GraphicsMode {
    pub(crate) max_mode: u32,
    pub(crate) mode: u32,
    pub(crate) info: *mut GraphicsModeInfo,
    pub(crate) size_of_info: usize,
    pub(crate) framebuffer_base: u64,
    pub(crate) framebuffer_size: usize,
}

/// The display.
#[repr(C)]
pub(crate) struct GraphicsOutput {
    query_mode: usize,
    set_mode: usize,
    blt: usize,
    pub(crate) mode: *mut GraphicsMode,
}

// ---------------------------------------------------------------------------
// Device path -- which disk the loader's own partition is on (A/B slots)
// ---------------------------------------------------------------------------

/// Identifies a handle's device path, a byte string of nodes.
pub(crate) const DEVICE_PATH_GUID: Guid = Guid::new(
    0x0957_6e91,
    0x6d3f,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);

// ---------------------------------------------------------------------------
// Block I/O -- the boot disk's GPT, the control record and the slots
// ---------------------------------------------------------------------------

/// Identifies [`BlockIo`].
pub(crate) const BLOCK_IO_GUID: Guid = Guid::new(
    0x964e_5b21,
    0x6459,
    0x11d2,
    [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
);

/// `EFI_BLOCK_IO_MEDIA`, the fields of revision 1 the loader reads.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockIoMedia {
    pub(crate) media_id: u32,
    pub(crate) removable_media: u8,
    pub(crate) media_present: u8,
    pub(crate) logical_partition: u8,
    pub(crate) read_only: u8,
    pub(crate) write_caching: u8,
    pub(crate) block_size: u32,
    pub(crate) io_align: u32,
    pub(crate) last_block: u64,
}

/// `EFI_BLOCK_IO_PROTOCOL`.
#[repr(C)]
pub(crate) struct BlockIo {
    pub(crate) revision: u64,
    pub(crate) media: *const BlockIoMedia,
    reset: usize,
    pub(crate) read_blocks: unsafe extern "efiapi" fn(
        this: *mut BlockIo,
        media_id: u32,
        lba: u64,
        buffer_size: usize,
        buffer: *mut u8,
    ) -> Status,
    pub(crate) write_blocks: unsafe extern "efiapi" fn(
        this: *mut BlockIo,
        media_id: u32,
        lba: u64,
        buffer_size: usize,
        buffer: *const u8,
    ) -> Status,
    pub(crate) flush_blocks: unsafe extern "efiapi" fn(this: *mut BlockIo) -> Status,
}
