//! DRM/KMS: the ioctls, constants and structures a software-rendered
//! compositor uses on `/dev/dri/card0`.
//!
//! This is the subset `docs/DISPLAY.md` §2.3 answers: the legacy
//! mode-setting calls, dumb buffers, page flips and the event records read
//! back from the card in iteration 1, and the plane and property queries
//! Smithay's legacy path makes in iteration 2 (E4). Atomic commit, setting
//! properties, property blobs, GEM names, PRIME and render nodes are later.
//!
//! # Where the numbers come from
//!
//! `include/uapi/drm/drm.h`, `drm_mode.h` and `drm_fourcc.h`, which every
//! architecture takes unchanged. `probe/drm.c` prints every number and layout
//! below from those headers, natively for 64-bit and under `qemu-arm` for
//! ARMv7-A, into `probe/drm-64.txt` and `probe/drm-32.txt`; the tests read
//! both files and require this module to agree with every line.
//!
//! # One structure has a width
//!
//! DRM's structures carry user pointers as `__u64`, so they are the same at
//! both widths, with one exception: `struct drm_version` holds three `size_t`
//! lengths and three `char *` pointers, which makes it 64 bytes on 64-bit and
//! 36 on ARMv7-A, and `DRM_IOCTL_VERSION`, whose number encodes the size,
//! differs with it. [`Version`] takes a [`Width`].
//!
//! # Reading and writing
//!
//! Every structure is read from and written into the bytes the ioctl's
//! argument points at, little-endian, returning `None` rather than
//! panicking when the buffer is short. What the fields mean is the kernel's
//! business; nothing here validates a value.

use crate::layout::layout;
pub use crate::layout::{Field, Layout};
use crate::socket::Width;

// ---------------------------------------------------------------------------
// ioctls
// ---------------------------------------------------------------------------

/// `DRM_IOCTL_VERSION` on 64-bit: `_IOWR('d', 0x00, struct drm_version)`.
pub const IOCTL_VERSION_64: u32 = 0xC040_6400;
/// `DRM_IOCTL_VERSION` on ARMv7-A, where `struct drm_version` is 36 bytes.
pub const IOCTL_VERSION_32: u32 = 0xC024_6400;
/// `DRM_IOCTL_GET_CAP`.
pub const IOCTL_GET_CAP: u32 = 0xC010_640C;
/// `DRM_IOCTL_SET_CLIENT_CAP`.
pub const IOCTL_SET_CLIENT_CAP: u32 = 0x4010_640D;
/// `DRM_IOCTL_SET_MASTER`, which takes no argument.
pub const IOCTL_SET_MASTER: u32 = 0x641E;
/// `DRM_IOCTL_DROP_MASTER`, which takes no argument.
pub const IOCTL_DROP_MASTER: u32 = 0x641F;
/// `DRM_IOCTL_MODE_GETRESOURCES`.
pub const IOCTL_MODE_GETRESOURCES: u32 = 0xC040_64A0;
/// `DRM_IOCTL_MODE_GETCRTC`.
pub const IOCTL_MODE_GETCRTC: u32 = 0xC068_64A1;
/// `DRM_IOCTL_MODE_SETCRTC`.
pub const IOCTL_MODE_SETCRTC: u32 = 0xC068_64A2;
/// `DRM_IOCTL_MODE_GETENCODER`.
pub const IOCTL_MODE_GETENCODER: u32 = 0xC014_64A6;
/// `DRM_IOCTL_MODE_GETCONNECTOR`.
pub const IOCTL_MODE_GETCONNECTOR: u32 = 0xC050_64A7;
/// `DRM_IOCTL_MODE_ADDFB`.
pub const IOCTL_MODE_ADDFB: u32 = 0xC01C_64AE;
/// `DRM_IOCTL_MODE_RMFB`, whose argument is a bare `unsigned int`.
pub const IOCTL_MODE_RMFB: u32 = 0xC004_64AF;
/// `DRM_IOCTL_MODE_PAGE_FLIP`.
pub const IOCTL_MODE_PAGE_FLIP: u32 = 0xC018_64B0;
/// `DRM_IOCTL_MODE_DIRTYFB`.
pub const IOCTL_MODE_DIRTYFB: u32 = 0xC018_64B1;
/// `DRM_IOCTL_MODE_CURSOR`: the cursor plane's image, or its place.
pub const IOCTL_MODE_CURSOR: u32 = 0xC01C_64A3;
/// `DRM_IOCTL_MODE_CURSOR2`: the same with a hotspot, which a virtual
/// card's host needs to draw the cursor where the pointer is.
pub const IOCTL_MODE_CURSOR2: u32 = 0xC024_64BB;
/// `DRM_IOCTL_MODE_CREATE_DUMB`.
pub const IOCTL_MODE_CREATE_DUMB: u32 = 0xC020_64B2;
/// `DRM_IOCTL_MODE_MAP_DUMB`.
pub const IOCTL_MODE_MAP_DUMB: u32 = 0xC010_64B3;
/// `DRM_IOCTL_MODE_DESTROY_DUMB`.
pub const IOCTL_MODE_DESTROY_DUMB: u32 = 0xC004_64B4;
/// `DRM_IOCTL_MODE_ADDFB2`.
pub const IOCTL_MODE_ADDFB2: u32 = 0xC068_64B8;
/// `DRM_IOCTL_MODE_GETPROPERTY`.
pub const IOCTL_MODE_GETPROPERTY: u32 = 0xC040_64AA;
/// `DRM_IOCTL_MODE_GETPROPBLOB`.
pub const IOCTL_MODE_GETPROPBLOB: u32 = 0xC010_64AC;
/// `DRM_IOCTL_MODE_GETPLANERESOURCES`.
pub const IOCTL_MODE_GETPLANERESOURCES: u32 = 0xC010_64B5;
/// `DRM_IOCTL_GEM_CLOSE`: let a handle go.
///
/// The one call that takes a buffer object away from an open. Without it an
/// open's objects live as long as the open does, which for a compositor is
/// as long as the session.
pub const IOCTL_GEM_CLOSE: u32 = 0x4008_6409;
/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`: a buffer object as a descriptor, which
/// another node of the same card can be given.
pub const IOCTL_PRIME_HANDLE_TO_FD: u32 = 0xC00C_642D;
/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: the other way about.
pub const IOCTL_PRIME_FD_TO_HANDLE: u32 = 0xC00C_642E;
/// `DRM_IOCTL_MODE_GETPLANE`.
pub const IOCTL_MODE_GETPLANE: u32 = 0xC020_64B6;
/// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`.
pub const IOCTL_MODE_OBJ_GETPROPERTIES: u32 = 0xC020_64B9;

/// `DRM_IOCTL_VERSION` at `width`.
#[must_use]
pub const fn ioctl_version(width: Width) -> u32 {
    match width {
        Width::Bits32 => IOCTL_VERSION_32,
        Width::Bits64 => IOCTL_VERSION_64,
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// `DRM_CAP_DUMB_BUFFER`: dumb buffers can be created.
pub const CAP_DUMB_BUFFER: u64 = 0x1;
/// `DRM_CAP_VBLANK_HIGH_CRTC`.
pub const CAP_VBLANK_HIGH_CRTC: u64 = 0x2;
/// `DRM_CAP_DUMB_PREFERRED_DEPTH`.
pub const CAP_DUMB_PREFERRED_DEPTH: u64 = 0x3;
/// `DRM_CAP_DUMB_PREFER_SHADOW`.
pub const CAP_DUMB_PREFER_SHADOW: u64 = 0x4;
/// `DRM_CAP_PRIME`.
pub const CAP_PRIME: u64 = 0x5;
/// `DRM_CAP_TIMESTAMP_MONOTONIC`: event timestamps are `CLOCK_MONOTONIC`.
pub const CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
/// `DRM_CAP_ASYNC_PAGE_FLIP`.
pub const CAP_ASYNC_PAGE_FLIP: u64 = 0x7;
/// `DRM_CAP_CURSOR_WIDTH`.
pub const CAP_CURSOR_WIDTH: u64 = 0x8;
/// `DRM_CAP_CURSOR_HEIGHT`.
pub const CAP_CURSOR_HEIGHT: u64 = 0x9;
/// `DRM_CAP_ADDFB2_MODIFIERS`.
pub const CAP_ADDFB2_MODIFIERS: u64 = 0x10;
/// `DRM_CAP_PAGE_FLIP_TARGET`.
pub const CAP_PAGE_FLIP_TARGET: u64 = 0x11;
/// `DRM_CAP_CRTC_IN_VBLANK_EVENT`: the event's `crtc_id` is filled in.
pub const CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
/// `DRM_CAP_SYNCOBJ`.
pub const CAP_SYNCOBJ: u64 = 0x13;
/// `DRM_CAP_SYNCOBJ_TIMELINE`.
pub const CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
/// `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`.
pub const CAP_ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;

/// `DRM_CLIENT_CAP_STEREO_3D`.
pub const CLIENT_CAP_STEREO_3D: u64 = 1;
/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES`.
pub const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
/// `DRM_CLIENT_CAP_ATOMIC`.
pub const CLIENT_CAP_ATOMIC: u64 = 3;
/// `DRM_CLIENT_CAP_ASPECT_RATIO`.
pub const CLIENT_CAP_ASPECT_RATIO: u64 = 4;
/// `DRM_CLIENT_CAP_WRITEBACK_CONNECTORS`.
pub const CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
/// `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`.
pub const CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// `DRM_EVENT_VBLANK`.
pub const EVENT_VBLANK: u32 = 0x01;
/// `DRM_EVENT_FLIP_COMPLETE`: a page flip asked for with
/// [`PAGE_FLIP_EVENT`] finished.
pub const EVENT_FLIP_COMPLETE: u32 = 0x02;
/// `DRM_EVENT_CRTC_SEQUENCE`.
pub const EVENT_CRTC_SEQUENCE: u32 = 0x03;
/// Ferrix's own: the card's connectors changed, so their modes are worth
/// asking for again. Eight bytes, the header alone.
///
/// Linux leaves event types from `0x8000_0000` to drivers
/// (`DRM_VMW_EVENT_FENCE_SIGNALED`, `DRM_EXYNOS_G2D_EVENT`), and libdrm's
/// `drmHandleEvent` steps over one it does not know by its length, so a
/// Linux program reading the card is not upset by it. Linux itself says a
/// connector changed with a udev uevent, which Ferrix does not send.
pub const EVENT_FERRIX_CONNECTORS: u32 = 0x8000_0000;

// ---------------------------------------------------------------------------
// Modes, connectors, encoders
// ---------------------------------------------------------------------------

/// `DRM_DISPLAY_MODE_LEN`: a mode's name field.
pub const DISPLAY_MODE_LEN: usize = 32;
/// `DRM_MODE_TYPE_PREFERRED`.
pub const MODE_TYPE_PREFERRED: u32 = 1 << 3;
/// `DRM_MODE_TYPE_USERDEF`.
pub const MODE_TYPE_USERDEF: u32 = 1 << 5;
/// `DRM_MODE_TYPE_DRIVER`.
pub const MODE_TYPE_DRIVER: u32 = 1 << 6;
/// `DRM_MODE_FLAG_PHSYNC`.
pub const MODE_FLAG_PHSYNC: u32 = 1 << 0;
/// `DRM_MODE_FLAG_NHSYNC`.
pub const MODE_FLAG_NHSYNC: u32 = 1 << 1;
/// `DRM_MODE_FLAG_PVSYNC`.
pub const MODE_FLAG_PVSYNC: u32 = 1 << 2;
/// `DRM_MODE_FLAG_NVSYNC`.
pub const MODE_FLAG_NVSYNC: u32 = 1 << 3;

/// `connection` of a connector with something attached: the kernel's
/// `connector_status_connected`, which `drm_mode.h` refers to but does not
/// export, so the probe cannot print it.
pub const CONNECTION_CONNECTED: u32 = 1;
/// `connector_status_disconnected`.
pub const CONNECTION_DISCONNECTED: u32 = 2;
/// `connector_status_unknown`.
pub const CONNECTION_UNKNOWN: u32 = 3;
/// `subpixel` when the order is unknown: the kernel's `SubPixelUnknown`,
/// likewise not exported.
pub const SUBPIXEL_UNKNOWN: u32 = 0;

/// `DRM_MODE_CONNECTOR_Unknown`.
pub const CONNECTOR_UNKNOWN: u32 = 0;
/// `DRM_MODE_CONNECTOR_VGA`.
pub const CONNECTOR_VGA: u32 = 1;
/// `DRM_MODE_CONNECTOR_DVII`.
pub const CONNECTOR_DVII: u32 = 2;
/// `DRM_MODE_CONNECTOR_DVID`.
pub const CONNECTOR_DVID: u32 = 3;
/// `DRM_MODE_CONNECTOR_LVDS`.
pub const CONNECTOR_LVDS: u32 = 7;
/// `DRM_MODE_CONNECTOR_DisplayPort`, which a program names `DP-1`.
pub const CONNECTOR_DISPLAYPORT: u32 = 10;
/// `DRM_MODE_CONNECTOR_eDP`.
pub const CONNECTOR_EDP: u32 = 14;
/// `DRM_MODE_CONNECTOR_DSI`.
pub const CONNECTOR_DSI: u32 = 16;
/// `DRM_MODE_CONNECTOR_USB`.
pub const CONNECTOR_USB: u32 = 20;
/// `DRM_MODE_CONNECTOR_VIRTUAL`: what virtio-gpu reports.
pub const CONNECTOR_VIRTUAL: u32 = 15;
/// `DRM_MODE_CONNECTOR_HDMIA`: an HDMI type A socket, which a program names
/// `HDMI-A-1`.
pub const CONNECTOR_HDMIA: u32 = 11;
/// `DRM_MODE_ENCODER_NONE`.
pub const ENCODER_NONE: u32 = 0;
/// `DRM_MODE_ENCODER_VIRTUAL`.
pub const ENCODER_VIRTUAL: u32 = 5;

// ---------------------------------------------------------------------------
// Framebuffers and flips
// ---------------------------------------------------------------------------

/// `DRM_MODE_FB_INTERLACED`.
pub const FB_INTERLACED: u32 = 1 << 0;
/// `DRM_MODE_FB_MODIFIERS`: `modifier` in `drm_mode_fb_cmd2` is valid.
pub const FB_MODIFIERS: u32 = 1 << 1;
/// `DRM_MODE_PAGE_FLIP_EVENT`: queue an event when the flip completes.
pub const PAGE_FLIP_EVENT: u32 = 0x01;
/// `DRM_MODE_PAGE_FLIP_ASYNC`.
pub const PAGE_FLIP_ASYNC: u32 = 0x02;
/// `DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE`.
pub const PAGE_FLIP_TARGET_ABSOLUTE: u32 = 0x4;
/// `DRM_MODE_PAGE_FLIP_TARGET_RELATIVE`.
pub const PAGE_FLIP_TARGET_RELATIVE: u32 = 0x8;
/// `DRM_MODE_FB_DIRTY_MAX_CLIPS`.
pub const FB_DIRTY_MAX_CLIPS: u32 = 256;
/// `DRM_MODE_CURSOR_BO`: a cursor call that sets the image.
pub const MODE_CURSOR_BO: u32 = 1;
/// `DRM_MODE_CURSOR_MOVE`: a cursor call that sets the place.
pub const MODE_CURSOR_MOVE: u32 = 2;
/// `DRM_MODE_CURSOR_FLAGS`: every flag a cursor call may carry.
pub const MODE_CURSOR_FLAGS: u32 = 3;

/// The fourcc code of `a`, `b`, `c`, `d`: `fourcc_code` in `drm_fourcc.h`.
#[must_use]
pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_le_bytes([a, b, c, d])
}

/// `DRM_FORMAT_XRGB8888`: `[31:0] x:R:G:B`, little-endian. The one format the
/// display answers in iteration 1, and what virtio-gpu calls
/// `B8G8R8X8_UNORM`.
pub const FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
/// `DRM_FORMAT_ARGB8888`.
pub const FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
/// `DRM_FORMAT_XBGR8888`.
pub const FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
/// `DRM_FORMAT_ABGR8888`.
pub const FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');

// ---------------------------------------------------------------------------
// Mode objects, planes and properties
// ---------------------------------------------------------------------------

/// `DRM_MODE_OBJECT_CRTC`: an object type, as `OBJ_GETPROPERTIES` names it.
pub const MODE_OBJECT_CRTC: u32 = 0xCCCC_CCCC;
/// `DRM_MODE_OBJECT_CONNECTOR`.
pub const MODE_OBJECT_CONNECTOR: u32 = 0xC0C0_C0C0;
/// `DRM_MODE_OBJECT_ENCODER`.
pub const MODE_OBJECT_ENCODER: u32 = 0xE0E0_E0E0;
/// `DRM_MODE_OBJECT_MODE`.
pub const MODE_OBJECT_MODE: u32 = 0xDEDE_DEDE;
/// `DRM_MODE_OBJECT_PROPERTY`.
pub const MODE_OBJECT_PROPERTY: u32 = 0xB0B0_B0B0;
/// `DRM_MODE_OBJECT_FB`.
pub const MODE_OBJECT_FB: u32 = 0xFBFB_FBFB;
/// `DRM_MODE_OBJECT_BLOB`.
pub const MODE_OBJECT_BLOB: u32 = 0xBBBB_BBBB;
/// `DRM_MODE_OBJECT_PLANE`.
pub const MODE_OBJECT_PLANE: u32 = 0xEEEE_EEEE;
/// `DRM_MODE_OBJECT_ANY`: whatever type the id has.
pub const MODE_OBJECT_ANY: u32 = 0;

/// `DRM_PROP_NAME_LEN`: a property's and an enum value's name field.
pub const PROP_NAME_LEN: usize = 32;
/// `DRM_MODE_PROP_PENDING`, deprecated.
pub const MODE_PROP_PENDING: u32 = 1 << 0;
/// `DRM_MODE_PROP_RANGE`.
pub const MODE_PROP_RANGE: u32 = 1 << 1;
/// `DRM_MODE_PROP_IMMUTABLE`: userspace cannot set it.
pub const MODE_PROP_IMMUTABLE: u32 = 1 << 2;
/// `DRM_MODE_PROP_ENUM`: the values are named in the enum list.
pub const MODE_PROP_ENUM: u32 = 1 << 3;
/// `DRM_MODE_PROP_BLOB`.
pub const MODE_PROP_BLOB: u32 = 1 << 4;
/// `DRM_MODE_PROP_BITMASK`.
pub const MODE_PROP_BITMASK: u32 = 1 << 5;
/// `DRM_MODE_PROP_LEGACY_TYPE`: the types that each have a bit.
pub const MODE_PROP_LEGACY_TYPE: u32 =
    MODE_PROP_RANGE | MODE_PROP_ENUM | MODE_PROP_BLOB | MODE_PROP_BITMASK;
/// `DRM_MODE_PROP_EXTENDED_TYPE`: the types numbered in bits 6 to 15.
pub const MODE_PROP_EXTENDED_TYPE: u32 = 0x0000_FFC0;
/// `DRM_MODE_PROP_OBJECT`: `DRM_MODE_PROP_TYPE(1)`.
pub const MODE_PROP_OBJECT: u32 = 1 << 6;
/// `DRM_MODE_PROP_SIGNED_RANGE`: `DRM_MODE_PROP_TYPE(2)`.
pub const MODE_PROP_SIGNED_RANGE: u32 = 2 << 6;
/// `DRM_MODE_PROP_ATOMIC`: hidden from clients that did not ask for atomic.
pub const MODE_PROP_ATOMIC: u32 = 0x8000_0000;

/// The `type` property's value for an overlay plane: the kernel's
/// `DRM_PLANE_TYPE_OVERLAY` in `enum drm_plane_type`
/// (`include/drm/drm_plane.h`), which the UAPI headers do not export, so the
/// probe cannot print it. The names are `drm_plane_type_enum_list`'s in
/// `drivers/gpu/drm/drm_mode_config.c`.
pub const PLANE_TYPE_OVERLAY: u64 = 0;
/// `DRM_PLANE_TYPE_PRIMARY`, named `Primary`.
pub const PLANE_TYPE_PRIMARY: u64 = 1;
/// `DRM_PLANE_TYPE_CURSOR`, named `Cursor`.
pub const PLANE_TYPE_CURSOR: u64 = 2;

// ---------------------------------------------------------------------------
// Layouts
// ---------------------------------------------------------------------------

layout! {
    /// `struct drm_gem_close`: `DRM_IOCTL_GEM_CLOSE`'s argument.
    GemClose = "drm_gem_close", 8 {
        /// The handle to let go of.
        handle: u32 = 0 / "handle",
        /// Written as zero.
        pad: u32 = 4 / "pad",
    }
}

layout! {
    /// `struct drm_prime_handle`: the argument of both PRIME calls.
    ///
    /// One structure for the two directions: `HANDLE_TO_FD` reads `handle`
    /// and writes `fd`, `FD_TO_HANDLE` reads `fd` and writes `handle`.
    PrimeHandle = "drm_prime_handle", 12 {
        /// The buffer object, on the node the call is made on.
        handle: u32 = 0 / "handle",
        /// `DRM_CLOEXEC` and `DRM_RDWR`, which are `O_CLOEXEC` and
        /// `O_RDWR`; `FD_TO_HANDLE` ignores them.
        flags: u32 = 4 / "flags",
        /// The descriptor.
        fd: i32 = 8 / "fd",
    }
}

layout! {
    /// `struct drm_get_cap`: `DRM_IOCTL_GET_CAP`'s argument.
    GetCap = "drm_get_cap", 16 {
        /// Which capability, a `DRM_CAP_*`.
        capability: u64 = 0 / "capability",
        /// Its value, filled in by the kernel.
        value: u64 = 8 / "value",
    }
}

layout! {
    /// `struct drm_set_client_cap`: `DRM_IOCTL_SET_CLIENT_CAP`'s argument.
    SetClientCap = "drm_set_client_cap", 16 {
        /// Which capability, a `DRM_CLIENT_CAP_*`.
        capability: u64 = 0 / "capability",
        /// The value asked for.
        value: u64 = 8 / "value",
    }
}

layout! {
    /// `struct drm_mode_modeinfo`: one display mode.
    ModeInfo = "drm_mode_modeinfo", 68 {
        /// Pixel clock in kHz.
        clock: u32 = 0 / "clock",
        /// Visible width.
        hdisplay: u16 = 4 / "hdisplay",
        /// Horizontal sync start.
        hsync_start: u16 = 6 / "hsync_start",
        /// Horizontal sync end.
        hsync_end: u16 = 8 / "hsync_end",
        /// Horizontal total.
        htotal: u16 = 10 / "htotal",
        /// Horizontal skew.
        hskew: u16 = 12 / "hskew",
        /// Visible height.
        vdisplay: u16 = 14 / "vdisplay",
        /// Vertical sync start.
        vsync_start: u16 = 16 / "vsync_start",
        /// Vertical sync end.
        vsync_end: u16 = 18 / "vsync_end",
        /// Vertical total.
        vtotal: u16 = 20 / "vtotal",
        /// Vertical scan.
        vscan: u16 = 22 / "vscan",
        /// Refresh rate in Hz.
        vrefresh: u32 = 24 / "vrefresh",
        /// `DRM_MODE_FLAG_*`.
        flags: u32 = 28 / "flags",
        /// `DRM_MODE_TYPE_*`.
        r#type: u32 = 32 / "type",
        /// The name, NUL-padded, such as `1280x800`.
        name: [u8; 32] = 36 / "name",
    }
}

layout! {
    /// `struct drm_mode_card_res`: `DRM_IOCTL_MODE_GETRESOURCES`'s argument.
    /// The pointers are user addresses of `u32` arrays the kernel fills when
    /// the counts the caller passed are large enough.
    CardRes = "drm_mode_card_res", 64 {
        /// Where to write framebuffer ids.
        fb_id_ptr: u64 = 0 / "fb_id_ptr",
        /// Where to write CRTC ids.
        crtc_id_ptr: u64 = 8 / "crtc_id_ptr",
        /// Where to write connector ids.
        connector_id_ptr: u64 = 16 / "connector_id_ptr",
        /// Where to write encoder ids.
        encoder_id_ptr: u64 = 24 / "encoder_id_ptr",
        /// Framebuffers: capacity in, count out.
        count_fbs: u32 = 32 / "count_fbs",
        /// CRTCs: capacity in, count out.
        count_crtcs: u32 = 36 / "count_crtcs",
        /// Connectors: capacity in, count out.
        count_connectors: u32 = 40 / "count_connectors",
        /// Encoders: capacity in, count out.
        count_encoders: u32 = 44 / "count_encoders",
        /// Smallest framebuffer width.
        min_width: u32 = 48 / "min_width",
        /// Largest framebuffer width.
        max_width: u32 = 52 / "max_width",
        /// Smallest framebuffer height.
        min_height: u32 = 56 / "min_height",
        /// Largest framebuffer height.
        max_height: u32 = 60 / "max_height",
    }
}

layout! {
    /// `struct drm_mode_crtc`: `DRM_IOCTL_MODE_GETCRTC` and `SETCRTC`.
    Crtc = "drm_mode_crtc", 104 {
        /// User address of the `u32` connector ids to drive.
        set_connectors_ptr: u64 = 0 / "set_connectors_ptr",
        /// How many ids are there.
        count_connectors: u32 = 8 / "count_connectors",
        /// The CRTC.
        crtc_id: u32 = 12 / "crtc_id",
        /// The framebuffer to scan out; 0 keeps the current one on `SETCRTC`
        /// with a mode, and turns the CRTC off without one.
        fb_id: u32 = 16 / "fb_id",
        /// Horizontal offset into the framebuffer.
        x: u32 = 20 / "x",
        /// Vertical offset into the framebuffer.
        y: u32 = 24 / "y",
        /// Gamma table size.
        gamma_size: u32 = 28 / "gamma_size",
        /// Whether `mode` is set.
        mode_valid: u32 = 32 / "mode_valid",
        /// The mode.
        mode: ModeInfo = 36 / "mode",
    }
}

layout! {
    /// `struct drm_mode_get_encoder`: `DRM_IOCTL_MODE_GETENCODER`.
    GetEncoder = "drm_mode_get_encoder", 20 {
        /// The encoder.
        encoder_id: u32 = 0 / "encoder_id",
        /// `DRM_MODE_ENCODER_*`.
        encoder_type: u32 = 4 / "encoder_type",
        /// The CRTC it is attached to, or 0.
        crtc_id: u32 = 8 / "crtc_id",
        /// A bit per CRTC index it can drive.
        possible_crtcs: u32 = 12 / "possible_crtcs",
        /// A bit per encoder index it can clone.
        possible_clones: u32 = 16 / "possible_clones",
    }
}

layout! {
    /// `struct drm_mode_get_connector`: `DRM_IOCTL_MODE_GETCONNECTOR`.
    GetConnector = "drm_mode_get_connector", 80 {
        /// Where to write encoder ids.
        encoders_ptr: u64 = 0 / "encoders_ptr",
        /// Where to write `struct drm_mode_modeinfo` records.
        modes_ptr: u64 = 8 / "modes_ptr",
        /// Where to write property ids.
        props_ptr: u64 = 16 / "props_ptr",
        /// Where to write property values.
        prop_values_ptr: u64 = 24 / "prop_values_ptr",
        /// Modes: capacity in, count out.
        count_modes: u32 = 32 / "count_modes",
        /// Properties: capacity in, count out.
        count_props: u32 = 36 / "count_props",
        /// Encoders: capacity in, count out.
        count_encoders: u32 = 40 / "count_encoders",
        /// The encoder currently attached, or 0.
        encoder_id: u32 = 44 / "encoder_id",
        /// The connector, as the caller asked.
        connector_id: u32 = 48 / "connector_id",
        /// `DRM_MODE_CONNECTOR_*`.
        connector_type: u32 = 52 / "connector_type",
        /// Its index among connectors of that type, from 1.
        connector_type_id: u32 = 56 / "connector_type_id",
        /// [`CONNECTION_CONNECTED`] and its siblings.
        connection: u32 = 60 / "connection",
        /// Physical width in millimetres.
        mm_width: u32 = 64 / "mm_width",
        /// Physical height in millimetres.
        mm_height: u32 = 68 / "mm_height",
        /// Subpixel order.
        subpixel: u32 = 72 / "subpixel",
        /// Padding.
        pad: u32 = 76 / "pad",
    }
}

layout! {
    /// `struct drm_mode_fb_cmd`: `DRM_IOCTL_MODE_ADDFB`.
    FbCmd = "drm_mode_fb_cmd", 28 {
        /// The new framebuffer's id, filled in by the kernel.
        fb_id: u32 = 0 / "fb_id",
        /// Width.
        width: u32 = 4 / "width",
        /// Height.
        height: u32 = 8 / "height",
        /// Bytes per row.
        pitch: u32 = 12 / "pitch",
        /// Bits per pixel.
        bpp: u32 = 16 / "bpp",
        /// Colour depth.
        depth: u32 = 20 / "depth",
        /// The buffer's handle.
        handle: u32 = 24 / "handle",
    }
}

layout! {
    /// `struct drm_mode_fb_cmd2`: `DRM_IOCTL_MODE_ADDFB2`.
    FbCmd2 = "drm_mode_fb_cmd2", 104 {
        /// The new framebuffer's id, filled in by the kernel.
        fb_id: u32 = 0 / "fb_id",
        /// Width.
        width: u32 = 4 / "width",
        /// Height.
        height: u32 = 8 / "height",
        /// A `DRM_FORMAT_*` fourcc.
        pixel_format: u32 = 12 / "pixel_format",
        /// `DRM_MODE_FB_*`.
        flags: u32 = 16 / "flags",
        /// A buffer handle per plane.
        handles: [u32; 4] = 20 / "handles",
        /// Bytes per row per plane.
        pitches: [u32; 4] = 36 / "pitches",
        /// Offset into the buffer per plane.
        offsets: [u32; 4] = 52 / "offsets",
        /// A format modifier per plane, when [`FB_MODIFIERS`] is set.
        modifier: [u64; 4] = 72 / "modifier",
    }
}

layout! {
    /// `struct drm_mode_crtc_page_flip`: `DRM_IOCTL_MODE_PAGE_FLIP`.
    CrtcPageFlip = "drm_mode_crtc_page_flip", 24 {
        /// The CRTC.
        crtc_id: u32 = 0 / "crtc_id",
        /// The framebuffer to show next.
        fb_id: u32 = 4 / "fb_id",
        /// `DRM_MODE_PAGE_FLIP_*`.
        flags: u32 = 8 / "flags",
        /// Must be zero.
        reserved: u32 = 12 / "reserved",
        /// Returned in the completion event.
        user_data: u64 = 16 / "user_data",
    }
}

layout! {
    /// `struct drm_mode_fb_dirty_cmd`: `DRM_IOCTL_MODE_DIRTYFB`.
    FbDirtyCmd = "drm_mode_fb_dirty_cmd", 24 {
        /// The framebuffer.
        fb_id: u32 = 0 / "fb_id",
        /// `DRM_MODE_FB_DIRTY_*`.
        flags: u32 = 4 / "flags",
        /// Fill colour, for the fill flag.
        color: u32 = 8 / "color",
        /// How many clip rectangles.
        num_clips: u32 = 12 / "num_clips",
        /// User address of the `struct drm_clip_rect` array.
        clips_ptr: u64 = 16 / "clips_ptr",
    }
}

layout! {
    /// `struct drm_mode_cursor`: `DRM_IOCTL_MODE_CURSOR`.
    ModeCursor = "drm_mode_cursor", 28 {
        /// `DRM_MODE_CURSOR_*`: what the call sets.
        flags: u32 = 0 / "flags",
        /// The CRTC whose cursor it is.
        crtc_id: u32 = 4 / "crtc_id",
        /// The image's left edge, for `MOVE`.
        x: i32 = 8 / "x",
        /// The image's top edge, for `MOVE`.
        y: i32 = 12 / "y",
        /// The image's width, for `BO`.
        width: u32 = 16 / "width",
        /// The image's height, for `BO`.
        height: u32 = 20 / "height",
        /// The dumb buffer holding the image, for `BO`; 0 for none.
        handle: u32 = 24 / "handle",
    }
}

layout! {
    /// `struct drm_mode_cursor2`: `DRM_IOCTL_MODE_CURSOR2`, which is
    /// `drm_mode_cursor` and a hotspot.
    ModeCursor2 = "drm_mode_cursor2", 36 {
        /// `DRM_MODE_CURSOR_*`: what the call sets.
        flags: u32 = 0 / "flags",
        /// The CRTC whose cursor it is.
        crtc_id: u32 = 4 / "crtc_id",
        /// The image's left edge, for `MOVE`.
        x: i32 = 8 / "x",
        /// The image's top edge, for `MOVE`.
        y: i32 = 12 / "y",
        /// The image's width, for `BO`.
        width: u32 = 16 / "width",
        /// The image's height, for `BO`.
        height: u32 = 20 / "height",
        /// The dumb buffer holding the image, for `BO`; 0 for none.
        handle: u32 = 24 / "handle",
        /// The hotspot, from the image's left edge.
        hot_x: i32 = 28 / "hot_x",
        /// The hotspot, from the image's top edge.
        hot_y: i32 = 32 / "hot_y",
    }
}

layout! {
    /// `struct drm_clip_rect`: a damaged rectangle, `x2` and `y2` exclusive.
    ClipRect = "drm_clip_rect", 8 {
        /// Left.
        x1: u16 = 0 / "x1",
        /// Top.
        y1: u16 = 2 / "y1",
        /// Right, exclusive.
        x2: u16 = 4 / "x2",
        /// Bottom, exclusive.
        y2: u16 = 6 / "y2",
    }
}

layout! {
    /// `struct drm_mode_create_dumb`: `DRM_IOCTL_MODE_CREATE_DUMB`.
    CreateDumb = "drm_mode_create_dumb", 32 {
        /// Height in pixels.
        height: u32 = 0 / "height",
        /// Width in pixels.
        width: u32 = 4 / "width",
        /// Bits per pixel.
        bpp: u32 = 8 / "bpp",
        /// Must be zero.
        flags: u32 = 12 / "flags",
        /// The new buffer's handle, filled in by the kernel.
        handle: u32 = 16 / "handle",
        /// Bytes per row, filled in by the kernel.
        pitch: u32 = 20 / "pitch",
        /// Bytes in all, filled in by the kernel.
        size: u64 = 24 / "size",
    }
}

layout! {
    /// `struct drm_mode_map_dumb`: `DRM_IOCTL_MODE_MAP_DUMB`.
    MapDumb = "drm_mode_map_dumb", 16 {
        /// The buffer.
        handle: u32 = 0 / "handle",
        /// Padding.
        pad: u32 = 4 / "pad",
        /// The offset to pass to `mmap`, filled in by the kernel.
        offset: u64 = 8 / "offset",
    }
}

layout! {
    /// `struct drm_mode_destroy_dumb`: `DRM_IOCTL_MODE_DESTROY_DUMB`.
    DestroyDumb = "drm_mode_destroy_dumb", 4 {
        /// The buffer.
        handle: u32 = 0 / "handle",
    }
}

layout! {
    /// `struct drm_mode_get_plane_res`: `DRM_IOCTL_MODE_GETPLANERESOURCES`.
    GetPlaneRes = "drm_mode_get_plane_res", 16 {
        /// Where to write plane ids.
        plane_id_ptr: u64 = 0 / "plane_id_ptr",
        /// Planes: capacity in, count out.
        count_planes: u32 = 8 / "count_planes",
    }
}

layout! {
    /// `struct drm_mode_get_plane`: `DRM_IOCTL_MODE_GETPLANE`.
    GetPlane = "drm_mode_get_plane", 32 {
        /// The plane, as the caller asked.
        plane_id: u32 = 0 / "plane_id",
        /// The CRTC it shows on, or 0.
        crtc_id: u32 = 4 / "crtc_id",
        /// The framebuffer it shows, or 0.
        fb_id: u32 = 8 / "fb_id",
        /// A bit per CRTC index it can be on.
        possible_crtcs: u32 = 12 / "possible_crtcs",
        /// Never used.
        gamma_size: u32 = 16 / "gamma_size",
        /// Formats: capacity in, count out.
        count_format_types: u32 = 20 / "count_format_types",
        /// Where to write the `u32` fourcc codes.
        format_type_ptr: u64 = 24 / "format_type_ptr",
    }
}

layout! {
    /// `struct drm_mode_obj_get_properties`:
    /// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`.
    ObjGetProperties = "drm_mode_obj_get_properties", 32 {
        /// Where to write `u32` property ids.
        props_ptr: u64 = 0 / "props_ptr",
        /// Where to write each property's `u64` value.
        prop_values_ptr: u64 = 8 / "prop_values_ptr",
        /// Properties: capacity in, count out.
        count_props: u32 = 16 / "count_props",
        /// The object.
        obj_id: u32 = 20 / "obj_id",
        /// Its `DRM_MODE_OBJECT_*` type, or [`MODE_OBJECT_ANY`].
        obj_type: u32 = 24 / "obj_type",
    }
}

layout! {
    /// `struct drm_mode_get_property`: `DRM_IOCTL_MODE_GETPROPERTY`.
    GetProperty = "drm_mode_get_property", 64 {
        /// Where to write the `u64` values.
        values_ptr: u64 = 0 / "values_ptr",
        /// Where to write `struct drm_mode_property_enum` records.
        enum_blob_ptr: u64 = 8 / "enum_blob_ptr",
        /// The property, as the caller asked.
        prop_id: u32 = 16 / "prop_id",
        /// `DRM_MODE_PROP_*`.
        flags: u32 = 20 / "flags",
        /// The name, NUL-padded.
        name: [u8; 32] = 24 / "name",
        /// Values: capacity in, count out.
        count_values: u32 = 56 / "count_values",
        /// Enum records: capacity in, count out.
        count_enum_blobs: u32 = 60 / "count_enum_blobs",
    }
}

layout! {
    /// `struct drm_mode_get_blob`: `DRM_IOCTL_MODE_GETPROPBLOB`.
    ///
    /// How a connector's `EDID` property is read: the property's value is a
    /// blob id, and this hands back the bytes. The call is made twice, as
    /// every counted DRM call is -- once with no buffer to learn the
    /// length, once with one that size.
    GetBlob = "drm_mode_get_blob", 16 {
        /// The blob, as the property's value named it.
        blob_id: u32 = 0 / "blob_id",
        /// Its length: capacity in, count out.
        length: u32 = 4 / "length",
        /// Where to write the bytes.
        data: u64 = 8 / "data",
    }
}

layout! {
    /// `struct drm_mode_property_enum`: one named value of an enum property.
    PropertyEnum = "drm_mode_property_enum", 40 {
        /// The value.
        value: u64 = 0 / "value",
        /// Its name, NUL-padded.
        name: [u8; 32] = 8 / "name",
    }
}

layout! {
    /// `struct drm_event`: the header of every record `read` returns.
    Event = "drm_event", 8 {
        /// `DRM_EVENT_*`.
        r#type: u32 = 0 / "type",
        /// The whole record's length, header included.
        length: u32 = 4 / "length",
    }
}

layout! {
    /// `struct drm_event_vblank`: a vblank or flip-complete record.
    EventVblank = "drm_event_vblank", 32 {
        /// The header.
        base: Event = 0 / "base",
        /// The `user_data` the page flip passed.
        user_data: u64 = 8 / "user_data",
        /// Seconds of the timestamp.
        tv_sec: u32 = 16 / "tv_sec",
        /// Microseconds of the timestamp.
        tv_usec: u32 = 20 / "tv_usec",
        /// The vblank counter.
        sequence: u32 = 24 / "sequence",
        /// The CRTC, when [`CAP_CRTC_IN_VBLANK_EVENT`] is set.
        crtc_id: u32 = 28 / "crtc_id",
    }
}

/// `struct drm_version`: `DRM_IOCTL_VERSION`'s argument, whose lengths and
/// pointers are `size_t` and `char *`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// Major version.
    pub version_major: i32,
    /// Minor version.
    pub version_minor: i32,
    /// Patch level.
    pub version_patchlevel: i32,
    /// Capacity of `name` in, the name's length out.
    pub name_len: u64,
    /// User address of the name buffer.
    pub name: u64,
    /// Capacity of `date` in, its length out.
    pub date_len: u64,
    /// User address of the date buffer.
    pub date: u64,
    /// Capacity of `desc` in, its length out.
    pub desc_len: u64,
    /// User address of the description buffer.
    pub desc: u64,
}

impl Version {
    /// The C name.
    pub const C_NAME: &'static str = "drm_version";

    /// `sizeof(struct drm_version)` at `width`.
    #[must_use]
    pub const fn size(width: Width) -> usize {
        match width {
            Width::Bits32 => 36,
            Width::Bits64 => 64,
        }
    }

    /// Every field's name and `offsetof` at `width`, in order: three `int`s,
    /// then three length and pointer pairs, each a word, aligned to a word.
    #[must_use]
    pub const fn fields(width: Width) -> [(&'static str, usize); 9] {
        let word = width.bytes();
        let base = 3 * 4 + (word - 3 * 4 % word) % word;
        [
            ("version_major", 0),
            ("version_minor", 4),
            ("version_patchlevel", 8),
            ("name_len", base),
            ("name", base + word),
            ("date_len", base + 2 * word),
            ("date", base + 3 * word),
            ("desc_len", base + 4 * word),
            ("desc", base + 5 * word),
        ]
    }

    /// Read it from the start of `bytes`.
    #[must_use]
    pub fn read(width: Width, bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::size(width) {
            return None;
        }
        let [_, _, _, name_len, name, date_len, date, desc_len, desc] = Self::fields(width);
        let word = |(_, at): (&str, usize)| width.word(bytes, at);
        Some(Self {
            version_major: i32::get(bytes, 0)?,
            version_minor: i32::get(bytes, 4)?,
            version_patchlevel: i32::get(bytes, 8)?,
            name_len: word(name_len)?,
            name: word(name)?,
            date_len: word(date_len)?,
            date: word(date)?,
            desc_len: word(desc_len)?,
            desc: word(desc)?,
        })
    }

    /// Write it into the start of `out`. A length or pointer too wide for a
    /// 32-bit word is refused rather than cut.
    pub fn write(&self, width: Width, out: &mut [u8]) -> Option<()> {
        if out.len() < Self::size(width) {
            return None;
        }
        let [_, _, _, name_len, name, date_len, date, desc_len, desc] = Self::fields(width);
        self.version_major.put(out, 0)?;
        self.version_minor.put(out, 4)?;
        self.version_patchlevel.put(out, 8)?;
        for ((_, at), value) in [
            (name_len, self.name_len),
            (name, self.name),
            (date_len, self.date_len),
            (date, self.date),
            (desc_len, self.desc_len),
            (desc, self.desc),
        ] {
            match width {
                Width::Bits32 => u32::try_from(value).ok()?.put(out, at)?,
                Width::Bits64 => value.put(out, at)?,
            }
        }
        Some(())
    }
}
