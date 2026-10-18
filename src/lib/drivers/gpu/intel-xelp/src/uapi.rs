// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, include/uapi/drm/i915_drm.h and
// include/uapi/drm/drm.h:
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas. (i915_drm.h)
// Copyright 1999 Precision Insight, Inc., Cedar Park, Texas.
// Copyright 2000 VA Linux Systems, Inc., Sunnyvale, California. (drm.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! The i915 uAPI's ioctl numbers and argument layouts, for the subset
//! `docs/INTEL-GPU.md` §2.1 serves. Each structure is read from and written
//! to the bytes the chardev core copies from and to the program, field by
//! field at its offset, little-endian, so no layout is left to a compiler.
//! The offsets and sizes are pinned by the tests against the numbers a probe
//! printed from Linux 7.0's headers (§2.1, Appendix A).

/// `_IOC(dir, type, nr, size)`, as `asm-generic/ioctl.h` packs it.
#[must_use]
pub const fn ioc(dir: u32, kind: u8, nr: u8, size: usize) -> u32 {
    (dir << 30) | ((size as u32 & 0x3fff) << 16) | ((kind as u32) << 8) | nr as u32
}

/// `_IOC_WRITE`.
pub const IOC_WRITE: u32 = 1;
/// `_IOC_READ`.
pub const IOC_READ: u32 = 2;
/// `DRM_IOCTL_BASE`, `'d'`.
pub const DRM_IOCTL_BASE: u8 = b'd';
/// `DRM_COMMAND_BASE`: driver ioctls start here.
pub const DRM_COMMAND_BASE: u8 = 0x40;

/// `DRM_IOWR(DRM_COMMAND_BASE + nr, size)`.
#[must_use]
pub const fn drm_iowr(nr: u8, size: usize) -> u32 {
    ioc(
        IOC_READ | IOC_WRITE,
        DRM_IOCTL_BASE,
        DRM_COMMAND_BASE + nr,
        size,
    )
}

/// `DRM_IOW(DRM_COMMAND_BASE + nr, size)`.
#[must_use]
pub const fn drm_iow(nr: u8, size: usize) -> u32 {
    ioc(IOC_WRITE, DRM_IOCTL_BASE, DRM_COMMAND_BASE + nr, size)
}

/// `DRM_IOR(DRM_COMMAND_BASE + nr, size)`.
#[must_use]
pub const fn drm_ior(nr: u8, size: usize) -> u32 {
    ioc(IOC_READ, DRM_IOCTL_BASE, DRM_COMMAND_BASE + nr, size)
}

/// A field of a uAPI structure.
pub trait Field: Copy {
    /// Its size in bytes.
    const SIZE: usize;
    /// Read it from `bytes` at `at`, little-endian.
    fn get(bytes: &[u8], at: usize) -> Option<Self>;
    /// Write it to `bytes` at `at`.
    fn put(self, bytes: &mut [u8], at: usize);
}

macro_rules! field {
    ($ty:ty, $n:literal) => {
        impl Field for $ty {
            const SIZE: usize = $n;
            fn get(bytes: &[u8], at: usize) -> Option<Self> {
                let slice = bytes.get(at..at + $n)?;
                let mut raw = [0_u8; $n];
                raw.copy_from_slice(slice);
                Some(<$ty>::from_le_bytes(raw))
            }
            fn put(self, bytes: &mut [u8], at: usize) {
                if let Some(slice) = bytes.get_mut(at..at + $n) {
                    slice.copy_from_slice(&self.to_le_bytes());
                }
            }
        }
    };
}

field!(u32, 4);
field!(i32, 4);
field!(u64, 8);
field!(i64, 8);

macro_rules! uapi_struct {
    ($(#[$meta:meta])* $name:ident, $size:literal { $($(#[$fmeta:meta])* $field:ident: $ty:ty = $at:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
        pub struct $name {
            $($(#[$fmeta])* pub $field: $ty,)*
        }

        impl $name {
            /// `sizeof`, as the ioctl number's size field carries it.
            pub const SIZE: usize = $size;

            /// Read it from the bytes copied in; `None` if they are short.
            #[must_use]
            pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() < Self::SIZE {
                    return None;
                }
                Some(Self { $($field: <$ty as Field>::get(bytes, $at)?,)* })
            }

            /// The bytes to copy out.
            #[must_use]
            pub fn to_bytes(&self) -> [u8; $size] {
                let mut bytes = [0_u8; $size];
                $(<$ty as Field>::put(self.$field, &mut bytes, $at);)*
                bytes
            }
        }
    };
}

uapi_struct!(
    /// `struct drm_i915_getparam`.
    GetParam, 16 {
        /// `param`, an `I915_PARAM_*`.
        param: i32 = 0,
        /// `value`: where the answer (an `int`) goes.
        value: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_query`.
    Query, 16 {
        /// `num_items`.
        num_items: u32 = 0,
        /// `flags`, which must be 0.
        flags: u32 = 4,
        /// `items_ptr`: an array of `drm_i915_query_item`.
        items_ptr: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_query_item`.
    QueryItem, 24 {
        /// `query_id`, a `DRM_I915_QUERY_*`.
        query_id: u64 = 0,
        /// `length`: 0 asks for the size; negative is an error back.
        length: i32 = 8,
        /// `flags`.
        flags: u32 = 12,
        /// `data_ptr`.
        data_ptr: u64 = 16,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_create`.
    GemCreate, 16 {
        /// `size`, rounded up to a page by the driver.
        size: u64 = 0,
        /// `handle`, out.
        handle: u32 = 8,
        /// `pad`.
        pad: u32 = 12,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_create_ext`.
    GemCreateExt, 24 {
        /// `size`.
        size: u64 = 0,
        /// `handle`, out.
        handle: u32 = 8,
        /// `flags`.
        flags: u32 = 12,
        /// `extensions`: a chain of `i915_user_extension`.
        extensions: u64 = 16,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_mmap_offset`.
    GemMmapOffset, 32 {
        /// `handle`.
        handle: u32 = 0,
        /// `pad`.
        pad: u32 = 4,
        /// `offset`, out: the fake offset to `mmap`.
        offset: u64 = 8,
        /// `flags`: `I915_MMAP_OFFSET_*`.
        flags: u64 = 16,
        /// `extensions`.
        extensions: u64 = 24,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_exec_object2`.
    ExecObject2, 56 {
        /// `handle`.
        handle: u32 = 0,
        /// `relocation_count`: must be 0 here (softpin only).
        relocation_count: u32 = 4,
        /// `relocs_ptr`.
        relocs_ptr: u64 = 8,
        /// `alignment`.
        alignment: u64 = 16,
        /// `offset`: the object's address in the context's VM.
        offset: u64 = 24,
        /// `flags`: `EXEC_OBJECT_*`.
        flags: u64 = 32,
        /// `rsvd1`, `pad_to_size`.
        rsvd1: u64 = 40,
        /// `rsvd2`.
        rsvd2: u64 = 48,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_execbuffer2`.
    Execbuffer2, 64 {
        /// `buffers_ptr`: an array of `drm_i915_gem_exec_object2`.
        buffers_ptr: u64 = 0,
        /// `buffer_count`.
        buffer_count: u32 = 8,
        /// `batch_start_offset`.
        batch_start_offset: u32 = 12,
        /// `batch_len`.
        batch_len: u32 = 16,
        /// `DR1`.
        dr1: u32 = 20,
        /// `DR4`.
        dr4: u32 = 24,
        /// `num_cliprects`: with `I915_EXEC_FENCE_ARRAY`, the fence count.
        num_cliprects: u32 = 28,
        /// `cliprects_ptr`: the fences, or the extension chain.
        cliprects_ptr: u64 = 32,
        /// `flags`: `I915_EXEC_*`.
        flags: u64 = 40,
        /// `rsvd1`: the context id.
        rsvd1: u64 = 48,
        /// `rsvd2`: the in and out fence descriptors.
        rsvd2: u64 = 56,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_context_create_ext`.
    ContextCreateExt, 16 {
        /// `ctx_id`, out.
        ctx_id: u32 = 0,
        /// `flags`.
        flags: u32 = 4,
        /// `extensions`.
        extensions: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_context_param`.
    ContextParam, 24 {
        /// `ctx_id`.
        ctx_id: u32 = 0,
        /// `size`.
        size: u32 = 4,
        /// `param`, an `I915_CONTEXT_PARAM_*`.
        param: u64 = 8,
        /// `value`.
        value: u64 = 16,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_vm_control`.
    VmControl, 16 {
        /// `extensions`.
        extensions: u64 = 0,
        /// `flags`.
        flags: u32 = 8,
        /// `vm_id`, out on create.
        vm_id: u32 = 12,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_wait`.
    GemWait, 16 {
        /// `bo_handle`.
        bo_handle: u32 = 0,
        /// `flags`.
        flags: u32 = 4,
        /// `timeout_ns`; negative waits for ever, and the time left goes back.
        timeout_ns: i64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_busy`.
    GemBusy, 8 {
        /// `handle`.
        handle: u32 = 0,
        /// `busy`, out: engine classes reading (high) and writing (low).
        busy: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_i915_reg_read`.
    RegRead, 16 {
        /// `offset`, with `I915_REG_READ_8B_WA` in bit 0.
        offset: u64 = 0,
        /// `val`, out.
        val: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_reset_stats`.
    ResetStats, 24 {
        /// `ctx_id`.
        ctx_id: u32 = 0,
        /// `flags`.
        flags: u32 = 4,
        /// `reset_count`: for the privileged only.
        reset_count: u32 = 8,
        /// `batch_active`: batches of this context that hung.
        batch_active: u32 = 12,
        /// `batch_pending`: batches lost to another's hang.
        batch_pending: u32 = 16,
        /// `pad`.
        pad: u32 = 20,
    }
);

uapi_struct!(
    /// `struct i915_user_extension`.
    UserExtension, 32 {
        /// `next_extension`.
        next_extension: u64 = 0,
        /// `name`.
        name: u32 = 8,
        /// `flags`.
        flags: u32 = 12,
        /// `rsvd[0]`.
        rsvd0: u32 = 16,
        /// `rsvd[1]`.
        rsvd1: u32 = 20,
        /// `rsvd[2]`.
        rsvd2: u32 = 24,
        /// `rsvd[3]`.
        rsvd3: u32 = 28,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_execbuffer_ext_timeline_fences`, whose first 32
    /// bytes are its `i915_user_extension`.
    ExecbufferExtTimelineFences, 56 {
        /// `base.next_extension`.
        next_extension: u64 = 0,
        /// `base.name`: `DRM_I915_GEM_EXECBUFFER_EXT_TIMELINE_FENCES`.
        name: u32 = 8,
        /// `base.flags`.
        flags: u32 = 12,
        /// `fence_count`.
        fence_count: u64 = 32,
        /// `handles_ptr`: `drm_i915_gem_exec_fence`s.
        handles_ptr: u64 = 40,
        /// `values_ptr`: the timeline points.
        values_ptr: u64 = 48,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_exec_fence`.
    ExecFence, 8 {
        /// `handle`: a syncobj.
        handle: u32 = 0,
        /// `flags`: `I915_EXEC_FENCE_WAIT`, `I915_EXEC_FENCE_SIGNAL`.
        flags: u32 = 4,
    }
);

/// The `DRM_I915_*` command numbers, from `DRM_COMMAND_BASE`.
pub mod nr {
    /// `DRM_I915_GETPARAM`.
    pub const GETPARAM: u8 = 0x06;
    /// `DRM_I915_GEM_BUSY`.
    pub const GEM_BUSY: u8 = 0x17;
    /// `DRM_I915_GEM_CREATE`.
    pub const GEM_CREATE: u8 = 0x1b;
    /// `DRM_I915_GEM_SET_DOMAIN`.
    pub const GEM_SET_DOMAIN: u8 = 0x1f;
    /// `DRM_I915_GEM_SET_TILING`.
    pub const GEM_SET_TILING: u8 = 0x21;
    /// `DRM_I915_GEM_GET_TILING`.
    pub const GEM_GET_TILING: u8 = 0x22;
    /// `DRM_I915_GEM_GET_APERTURE`.
    pub const GEM_GET_APERTURE: u8 = 0x23;
    /// `DRM_I915_GEM_MMAP_GTT`, which `MMAP_OFFSET` shares.
    pub const GEM_MMAP_OFFSET: u8 = 0x24;
    /// `DRM_I915_GEM_MADVISE`.
    pub const GEM_MADVISE: u8 = 0x26;
    /// `DRM_I915_GEM_EXECBUFFER2`.
    pub const GEM_EXECBUFFER2: u8 = 0x29;
    /// `DRM_I915_GEM_WAIT`.
    pub const GEM_WAIT: u8 = 0x2c;
    /// `DRM_I915_GEM_CONTEXT_CREATE`.
    pub const GEM_CONTEXT_CREATE: u8 = 0x2d;
    /// `DRM_I915_GEM_CONTEXT_DESTROY`.
    pub const GEM_CONTEXT_DESTROY: u8 = 0x2e;
    /// `DRM_I915_GEM_SET_CACHING`.
    pub const GEM_SET_CACHING: u8 = 0x2f;
    /// `DRM_I915_REG_READ`.
    pub const REG_READ: u8 = 0x31;
    /// `DRM_I915_GET_RESET_STATS`.
    pub const GET_RESET_STATS: u8 = 0x32;
    /// `DRM_I915_GEM_USERPTR`.
    pub const GEM_USERPTR: u8 = 0x33;
    /// `DRM_I915_GEM_CONTEXT_GETPARAM`.
    pub const GEM_CONTEXT_GETPARAM: u8 = 0x34;
    /// `DRM_I915_GEM_CONTEXT_SETPARAM`.
    pub const GEM_CONTEXT_SETPARAM: u8 = 0x35;
    /// `DRM_I915_QUERY`.
    pub const QUERY: u8 = 0x39;
    /// `DRM_I915_GEM_VM_CREATE`.
    pub const GEM_VM_CREATE: u8 = 0x3a;
    /// `DRM_I915_GEM_VM_DESTROY`.
    pub const GEM_VM_DESTROY: u8 = 0x3b;
    /// `DRM_I915_GEM_CREATE_EXT`.
    pub const GEM_CREATE_EXT: u8 = 0x3c;
}

/// The full ioctl numbers, as `DRM_IOCTL_I915_*`.
pub mod ioctl {
    use super::{
        ContextCreateExt, ContextParam, Execbuffer2, GemBusy, GemCreate, GemCreateExt,
        GemMmapOffset, GemWait, GetParam, Query, RegRead, ResetStats, VmControl, drm_ior, drm_iow,
        drm_iowr, nr,
    };

    /// `DRM_IOCTL_I915_GETPARAM`.
    pub const GETPARAM: u32 = drm_iowr(nr::GETPARAM, GetParam::SIZE);
    /// `DRM_IOCTL_I915_QUERY`.
    pub const QUERY: u32 = drm_iowr(nr::QUERY, Query::SIZE);
    /// `DRM_IOCTL_I915_GEM_CREATE`.
    pub const GEM_CREATE: u32 = drm_iowr(nr::GEM_CREATE, GemCreate::SIZE);
    /// `DRM_IOCTL_I915_GEM_CREATE_EXT`.
    pub const GEM_CREATE_EXT: u32 = drm_iowr(nr::GEM_CREATE_EXT, GemCreateExt::SIZE);
    /// `DRM_IOCTL_I915_GEM_MMAP_OFFSET`.
    pub const GEM_MMAP_OFFSET: u32 = drm_iowr(nr::GEM_MMAP_OFFSET, GemMmapOffset::SIZE);
    /// `DRM_IOCTL_I915_GEM_EXECBUFFER2`.
    pub const GEM_EXECBUFFER2: u32 = drm_iow(nr::GEM_EXECBUFFER2, Execbuffer2::SIZE);
    /// `DRM_IOCTL_I915_GEM_EXECBUFFER2_WR`.
    pub const GEM_EXECBUFFER2_WR: u32 = drm_iowr(nr::GEM_EXECBUFFER2, Execbuffer2::SIZE);
    /// `DRM_IOCTL_I915_GEM_WAIT`.
    pub const GEM_WAIT: u32 = drm_iowr(nr::GEM_WAIT, GemWait::SIZE);
    /// `DRM_IOCTL_I915_GEM_BUSY`.
    pub const GEM_BUSY: u32 = drm_iowr(nr::GEM_BUSY, GemBusy::SIZE);
    /// `DRM_IOCTL_I915_GEM_CONTEXT_CREATE_EXT` (and `CONTEXT_CREATE`, whose
    /// 8-byte argument is the same structure's first half).
    pub const GEM_CONTEXT_CREATE_EXT: u32 =
        drm_iowr(nr::GEM_CONTEXT_CREATE, ContextCreateExt::SIZE);
    /// `DRM_IOCTL_I915_GEM_CONTEXT_CREATE`.
    pub const GEM_CONTEXT_CREATE: u32 = drm_iowr(nr::GEM_CONTEXT_CREATE, 8);
    /// `DRM_IOCTL_I915_GEM_CONTEXT_DESTROY`.
    pub const GEM_CONTEXT_DESTROY: u32 = drm_iow(nr::GEM_CONTEXT_DESTROY, 8);
    /// `DRM_IOCTL_I915_GEM_CONTEXT_GETPARAM`.
    pub const GEM_CONTEXT_GETPARAM: u32 = drm_iowr(nr::GEM_CONTEXT_GETPARAM, ContextParam::SIZE);
    /// `DRM_IOCTL_I915_GEM_CONTEXT_SETPARAM`.
    pub const GEM_CONTEXT_SETPARAM: u32 = drm_iowr(nr::GEM_CONTEXT_SETPARAM, ContextParam::SIZE);
    /// `DRM_IOCTL_I915_GEM_VM_CREATE`.
    pub const GEM_VM_CREATE: u32 = drm_iowr(nr::GEM_VM_CREATE, VmControl::SIZE);
    /// `DRM_IOCTL_I915_GEM_VM_DESTROY`.
    pub const GEM_VM_DESTROY: u32 = drm_iow(nr::GEM_VM_DESTROY, VmControl::SIZE);
    /// `DRM_IOCTL_I915_GET_RESET_STATS`.
    pub const GET_RESET_STATS: u32 = drm_iowr(nr::GET_RESET_STATS, ResetStats::SIZE);
    /// `DRM_IOCTL_I915_REG_READ`.
    pub const REG_READ: u32 = drm_iowr(nr::REG_READ, RegRead::SIZE);
    /// `DRM_IOCTL_I915_GEM_SET_TILING`.
    pub const GEM_SET_TILING: u32 = drm_iowr(nr::GEM_SET_TILING, 16);
    /// `DRM_IOCTL_I915_GEM_GET_TILING`.
    pub const GEM_GET_TILING: u32 = drm_iowr(nr::GEM_GET_TILING, 16);
    /// `DRM_IOCTL_I915_GEM_SET_DOMAIN`.
    pub const GEM_SET_DOMAIN: u32 = drm_iow(nr::GEM_SET_DOMAIN, 12);
    /// `DRM_IOCTL_I915_GEM_SET_CACHING`.
    pub const GEM_SET_CACHING: u32 = drm_iow(nr::GEM_SET_CACHING, 8);
    /// `DRM_IOCTL_I915_GEM_MADVISE`.
    pub const GEM_MADVISE: u32 = drm_iowr(nr::GEM_MADVISE, 12);
    /// `DRM_IOCTL_I915_GEM_USERPTR`.
    pub const GEM_USERPTR: u32 = drm_iowr(nr::GEM_USERPTR, 24);
    /// `DRM_IOCTL_I915_GEM_GET_APERTURE`.
    pub const GEM_GET_APERTURE: u32 = drm_ior(nr::GEM_GET_APERTURE, 16);
}
