// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, include/uapi/drm/i915_drm.h,
// include/uapi/drm/drm.h and include/uapi/linux/dma-buf.h:
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas. (i915_drm.h)
// Copyright 1999 Precision Insight, Inc., Cedar Park, Texas.
// Copyright 2000 VA Linux Systems, Inc., Sunnyvale, California. (drm.h)
// Under the MIT notice in this crate's i915/NOTICE. dma-buf.h's numbers are
// facts of the interface, not its text.

//! The i915 uAPI's ioctl numbers, constants and argument layouts, for the
//! subset `docs/INTEL-GPU.md` §2.1 serves: the common DRM ioctls a render
//! node answers, syncobjs, i915's own, and the two dma-buf sync-file ioctls.
//!
//! Each structure is read from and written to the bytes the chardev core
//! copies from and to the program, field by field at its offset,
//! little-endian, so no layout is left to a compiler. `probe/i915.c` prints
//! every number and layout here from Linux v7.0's own headers into
//! `probe/i915-64.txt`, and the tests require this module to agree with every
//! line of it, both ways ([`LAYOUTS`], [`VALUES`]). No structure here has a
//! width: every pointer is a `__u64`.

/// `_IOC(dir, type, nr, size)`, as `asm-generic/ioctl.h` packs it.
#[must_use]
pub const fn ioc(dir: u32, kind: u8, nr: u8, size: usize) -> u32 {
    (dir << 30) | ((size as u32 & 0x3fff) << 16) | ((kind as u32) << 8) | nr as u32
}

/// `_IOC_WRITE`: the program writes the argument (the kernel copies in).
pub const IOC_WRITE: u32 = 1;
/// `_IOC_READ`: the kernel copies out.
pub const IOC_READ: u32 = 2;
/// `DRM_IOCTL_BASE`, `'d'`.
pub const DRM_IOCTL_BASE: u8 = b'd';
/// `DRM_COMMAND_BASE`: driver ioctls start here.
pub const DRM_COMMAND_BASE: u8 = 0x40;
/// `DRM_COMMAND_END`.
pub const DRM_COMMAND_END: u8 = 0xa0;

/// `_IOC_NR(cmd)`.
#[must_use]
pub const fn ioc_nr(cmd: u32) -> u8 {
    cmd as u8
}

/// `_IOC_TYPE(cmd)`.
#[must_use]
pub const fn ioc_type(cmd: u32) -> u8 {
    (cmd >> 8) as u8
}

/// `_IOC_SIZE(cmd)`.
#[must_use]
pub const fn ioc_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

/// `_IOC_DIR(cmd)`.
#[must_use]
pub const fn ioc_dir(cmd: u32) -> u32 {
    cmd >> 30
}

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

field!(u16, 2);
field!(u32, 4);
field!(i32, 4);
field!(u64, 8);
field!(i64, 8);

/// A structure's C name, size and field offsets, for the probe's test.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// `struct <name>`.
    pub name: &'static str,
    /// `sizeof`.
    pub size: usize,
    /// Each C field this module names, with its `offsetof`.
    pub fields: &'static [(&'static str, usize)],
}

macro_rules! c_name {
    ($field:ident) => {
        stringify!($field)
    };
    ($field:ident, $c:literal) => {
        $c
    };
}

macro_rules! uapi_struct {
    ($(#[$meta:meta])* $name:ident = $c:literal, $size:literal {
        $($(#[$fmeta:meta])* $field:ident: $ty:ty = $at:literal $(as $cfield:literal)?,)*
    }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
        pub struct $name {
            $($(#[$fmeta])* pub $field: $ty,)*
        }

        impl $name {
            /// `sizeof`, as the ioctl number's size field carries it.
            pub const SIZE: usize = $size;

            /// Its C name and layout.
            pub const LAYOUT: Layout = Layout {
                name: $c,
                size: $size,
                fields: &[$((c_name!($field $(, $cfield)?), $at),)*],
            };

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

// ---------------------------------------------------------------------------
// Common DRM structures (drm.h)
// ---------------------------------------------------------------------------

uapi_struct!(
    /// `struct drm_version`, 64-bit: three `int`s, then three
    /// (`size_t` length, `char *`) pairs. Not printed by the probe (its
    /// layout has a width); `src/lib/proto/linux-abi`'s `drm.rs` pins the
    /// same numbers against its own probe at both widths.
    Version = "drm_version", 64 {
        /// `version_major`.
        version_major: i32 = 0,
        /// `version_minor`.
        version_minor: i32 = 4,
        /// `version_patchlevel`.
        version_patchlevel: i32 = 8,
        /// `name_len`.
        name_len: u64 = 16,
        /// `name`.
        name: u64 = 24,
        /// `date_len`.
        date_len: u64 = 32,
        /// `date`.
        date: u64 = 40,
        /// `desc_len`.
        desc_len: u64 = 48,
        /// `desc`.
        desc: u64 = 56,
    }
);

uapi_struct!(
    /// `struct drm_get_cap` (and `drm_set_client_cap`, the same layout).
    GetCap = "drm_get_cap", 16 {
        /// `capability`.
        capability: u64 = 0,
        /// `value`.
        value: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_gem_close`.
    GemClose = "drm_gem_close", 8 {
        /// `handle`.
        handle: u32 = 0,
        /// `pad`.
        pad: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_prime_handle`.
    PrimeHandle = "drm_prime_handle", 12 {
        /// `handle`.
        handle: u32 = 0,
        /// `flags`: `DRM_CLOEXEC`, `DRM_RDWR`.
        flags: u32 = 4,
        /// `fd`.
        fd: i32 = 8,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_create`.
    SyncobjCreate = "drm_syncobj_create", 8 {
        /// `handle`, out.
        handle: u32 = 0,
        /// `flags`: `DRM_SYNCOBJ_CREATE_SIGNALED`.
        flags: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_destroy`.
    SyncobjDestroy = "drm_syncobj_destroy", 8 {
        /// `handle`.
        handle: u32 = 0,
        /// `pad`, which must be zero.
        pad: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_handle`.
    SyncobjHandle = "drm_syncobj_handle", 24 {
        /// `handle`.
        handle: u32 = 0,
        /// `flags`.
        flags: u32 = 4,
        /// `fd`.
        fd: i32 = 8,
        /// `pad`.
        pad: u32 = 12,
        /// `point`, with the `TIMELINE` flags.
        point: u64 = 16,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_wait`.
    SyncobjWait = "drm_syncobj_wait", 40 {
        /// `handles`: an array of `__u32`.
        handles: u64 = 0,
        /// `timeout_nsec`: an absolute `CLOCK_MONOTONIC` time.
        timeout_nsec: i64 = 8,
        /// `count_handles`.
        count_handles: u32 = 16,
        /// `flags`.
        flags: u32 = 20,
        /// `first_signaled`, out, for a wait for any.
        first_signaled: u32 = 24,
        /// `pad`.
        pad: u32 = 28,
        /// `deadline_nsec`, with `WAIT_DEADLINE`.
        deadline_nsec: u64 = 32,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_timeline_wait`.
    SyncobjTimelineWait = "drm_syncobj_timeline_wait", 48 {
        /// `handles`.
        handles: u64 = 0,
        /// `points`: an array of `__u64`.
        points: u64 = 8,
        /// `timeout_nsec`, absolute.
        timeout_nsec: i64 = 16,
        /// `count_handles`.
        count_handles: u32 = 24,
        /// `flags`.
        flags: u32 = 28,
        /// `first_signaled`, out.
        first_signaled: u32 = 32,
        /// `pad`.
        pad: u32 = 36,
        /// `deadline_nsec`.
        deadline_nsec: u64 = 40,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_array`.
    SyncobjArray = "drm_syncobj_array", 16 {
        /// `handles`.
        handles: u64 = 0,
        /// `count_handles`.
        count_handles: u32 = 8,
        /// `pad`, which must be zero.
        pad: u32 = 12,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_timeline_array`.
    SyncobjTimelineArray = "drm_syncobj_timeline_array", 24 {
        /// `handles`.
        handles: u64 = 0,
        /// `points`.
        points: u64 = 8,
        /// `count_handles`.
        count_handles: u32 = 16,
        /// `flags`.
        flags: u32 = 20,
    }
);

uapi_struct!(
    /// `struct drm_syncobj_transfer`.
    SyncobjTransfer = "drm_syncobj_transfer", 32 {
        /// `src_handle`.
        src_handle: u32 = 0,
        /// `dst_handle`.
        dst_handle: u32 = 4,
        /// `src_point`.
        src_point: u64 = 8,
        /// `dst_point`.
        dst_point: u64 = 16,
        /// `flags`.
        flags: u32 = 24,
        /// `pad`, which must be zero.
        pad: u32 = 28,
    }
);

// ---------------------------------------------------------------------------
// i915's structures (i915_drm.h)
// ---------------------------------------------------------------------------

uapi_struct!(
    /// `struct drm_i915_getparam`.
    GetParam = "drm_i915_getparam", 16 {
        /// `param`, an `I915_PARAM_*`.
        param: i32 = 0,
        /// `value`: where the answer (an `int`) goes.
        value: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_query`.
    Query = "drm_i915_query", 16 {
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
    QueryItem = "drm_i915_query_item", 24 {
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
    /// `struct drm_i915_query_topology_info`'s header; `data[]` follows.
    TopologyInfo = "drm_i915_query_topology_info", 16 {
        /// `flags`.
        flags: u16 = 0,
        /// `max_slices`.
        max_slices: u16 = 2,
        /// `max_subslices`.
        max_subslices: u16 = 4,
        /// `max_eus_per_subslice`.
        max_eus_per_subslice: u16 = 6,
        /// `subslice_offset`.
        subslice_offset: u16 = 8,
        /// `subslice_stride`.
        subslice_stride: u16 = 10,
        /// `eu_offset`.
        eu_offset: u16 = 12,
        /// `eu_stride`.
        eu_stride: u16 = 14,
    }
);

uapi_struct!(
    /// `struct i915_engine_class_instance`.
    EngineClassInstance = "i915_engine_class_instance", 4 {
        /// `engine_class`, an `I915_ENGINE_CLASS_*`.
        engine_class: u16 = 0,
        /// `engine_instance`.
        engine_instance: u16 = 2,
    }
);

uapi_struct!(
    /// `struct drm_i915_engine_info`.
    EngineInfo = "drm_i915_engine_info", 56 {
        /// `engine.engine_class`.
        engine_class: u16 = 0 as "engine",
        /// `engine.engine_instance`.
        engine_instance: u16 = 2 as "engine.engine_instance",
        /// `rsvd0`.
        rsvd0: u32 = 4,
        /// `flags`.
        flags: u64 = 8,
        /// `capabilities`.
        capabilities: u64 = 16,
        /// `logical_instance`.
        logical_instance: u16 = 24,
    }
);

uapi_struct!(
    /// `struct drm_i915_query_engine_info`'s header; `engines[]` follows.
    QueryEngineInfo = "drm_i915_query_engine_info", 16 {
        /// `num_engines`.
        num_engines: u32 = 0,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_memory_class_instance`.
    MemoryClassInstance = "drm_i915_gem_memory_class_instance", 4 {
        /// `memory_class`.
        memory_class: u16 = 0,
        /// `memory_instance`.
        memory_instance: u16 = 2,
    }
);

uapi_struct!(
    /// `struct drm_i915_memory_region_info`.
    MemoryRegionInfo = "drm_i915_memory_region_info", 88 {
        /// `region.memory_class`.
        memory_class: u16 = 0 as "region",
        /// `region.memory_instance`.
        memory_instance: u16 = 2 as "region.memory_instance",
        /// `probed_size`.
        probed_size: u64 = 8,
        /// `unallocated_size`.
        unallocated_size: u64 = 16,
        /// `probed_cpu_visible_size`.
        probed_cpu_visible_size: u64 = 24,
        /// `unallocated_cpu_visible_size`.
        unallocated_cpu_visible_size: u64 = 32,
    }
);

uapi_struct!(
    /// `struct drm_i915_query_memory_regions`'s header; `regions[]` follows.
    QueryMemoryRegions = "drm_i915_query_memory_regions", 16 {
        /// `num_regions`.
        num_regions: u32 = 0,
    }
);

uapi_struct!(
    /// `struct i915_user_extension`.
    UserExtension = "i915_user_extension", 32 {
        /// `next_extension`.
        next_extension: u64 = 0,
        /// `name`.
        name: u32 = 8,
        /// `flags`, which must be zero.
        flags: u32 = 12,
        /// `rsvd[0]`.
        rsvd0: u32 = 16 as "rsvd",
        /// `rsvd[1]`.
        rsvd1: u32 = 20 as "rsvd[1]",
        /// `rsvd[2]`.
        rsvd2: u32 = 24 as "rsvd[2]",
        /// `rsvd[3]`.
        rsvd3: u32 = 28 as "rsvd[3]",
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_create`.
    GemCreate = "drm_i915_gem_create", 16 {
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
    GemCreateExt = "drm_i915_gem_create_ext", 24 {
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
    /// `struct drm_i915_gem_create_ext_memory_regions`, whose first 32
    /// bytes are its `i915_user_extension`.
    CreateExtMemoryRegions = "drm_i915_gem_create_ext_memory_regions", 48 {
        /// `pad`.
        pad: u32 = 32,
        /// `num_regions`.
        num_regions: u32 = 36,
        /// `regions`: `drm_i915_gem_memory_class_instance`s.
        regions: u64 = 40,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_create_ext_set_pat`.
    CreateExtSetPat = "drm_i915_gem_create_ext_set_pat", 40 {
        /// `pat_index`.
        pat_index: u32 = 32,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_mmap_offset`.
    GemMmapOffset = "drm_i915_gem_mmap_offset", 32 {
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
    ExecObject2 = "drm_i915_gem_exec_object2", 56 {
        /// `handle`.
        handle: u32 = 0,
        /// `relocation_count`: must be 0 on Gen12 past TGL.
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
    Execbuffer2 = "drm_i915_gem_execbuffer2", 64 {
        /// `buffers_ptr`: an array of `drm_i915_gem_exec_object2`.
        buffers_ptr: u64 = 0,
        /// `buffer_count`.
        buffer_count: u32 = 8,
        /// `batch_start_offset`.
        batch_start_offset: u32 = 12,
        /// `batch_len`.
        batch_len: u32 = 16,
        /// `DR1`.
        dr1: u32 = 20 as "DR1",
        /// `DR4`.
        dr4: u32 = 24 as "DR4",
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
    /// `struct drm_i915_gem_exec_fence`.
    ExecFence = "drm_i915_gem_exec_fence", 8 {
        /// `handle`: a syncobj.
        handle: u32 = 0,
        /// `flags`: `I915_EXEC_FENCE_WAIT`, `I915_EXEC_FENCE_SIGNAL`.
        flags: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_execbuffer_ext_timeline_fences`, whose first 32
    /// bytes are its `i915_user_extension`.
    ExecbufferExtTimelineFences = "drm_i915_gem_execbuffer_ext_timeline_fences", 56 {
        /// `base.next_extension`.
        next_extension: u64 = 0 as "base",
        /// `base.name`: `DRM_I915_GEM_EXECBUFFER_EXT_TIMELINE_FENCES`.
        name: u32 = 8 as "base.name",
        /// `base.flags`.
        flags: u32 = 12 as "base.flags",
        /// `fence_count`.
        fence_count: u64 = 32,
        /// `handles_ptr`: `drm_i915_gem_exec_fence`s.
        handles_ptr: u64 = 40,
        /// `values_ptr`: the timeline points.
        values_ptr: u64 = 48,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_wait`.
    GemWait = "drm_i915_gem_wait", 16 {
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
    GemBusy = "drm_i915_gem_busy", 8 {
        /// `handle`.
        handle: u32 = 0,
        /// `busy`, out: engine classes reading (high) and writing (low).
        busy: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_set_tiling`.
    SetTiling = "drm_i915_gem_set_tiling", 16 {
        /// `handle`.
        handle: u32 = 0,
        /// `tiling_mode`.
        tiling_mode: u32 = 4,
        /// `stride`.
        stride: u32 = 8,
        /// `swizzle_mode`, out.
        swizzle_mode: u32 = 12,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_get_tiling`.
    GetTiling = "drm_i915_gem_get_tiling", 16 {
        /// `handle`.
        handle: u32 = 0,
        /// `tiling_mode`, out.
        tiling_mode: u32 = 4,
        /// `swizzle_mode`, out.
        swizzle_mode: u32 = 8,
        /// `phys_swizzle_mode`, out.
        phys_swizzle_mode: u32 = 12,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_caching`.
    Caching = "drm_i915_gem_caching", 8 {
        /// `handle`.
        handle: u32 = 0,
        /// `caching`: `I915_CACHING_*`.
        caching: u32 = 4,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_set_domain`.
    SetDomain = "drm_i915_gem_set_domain", 12 {
        /// `handle`.
        handle: u32 = 0,
        /// `read_domains`.
        read_domains: u32 = 4,
        /// `write_domain`.
        write_domain: u32 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_madvise`.
    Madvise = "drm_i915_gem_madvise", 12 {
        /// `handle`.
        handle: u32 = 0,
        /// `madv`.
        madv: u32 = 4,
        /// `retained`, out.
        retained: u32 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_get_aperture`.
    GetAperture = "drm_i915_gem_get_aperture", 16 {
        /// `aper_size`.
        aper_size: u64 = 0,
        /// `aper_available_size`.
        aper_available_size: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_context_create_ext` (and, its first eight
    /// bytes, `drm_i915_gem_context_create`).
    ContextCreateExt = "drm_i915_gem_context_create_ext", 16 {
        /// `ctx_id`, out.
        ctx_id: u32 = 0,
        /// `flags`.
        flags: u32 = 4,
        /// `extensions`.
        extensions: u64 = 8,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_context_create_ext_setparam`: an
    /// `i915_user_extension`, then a `drm_i915_gem_context_param`.
    ContextCreateExtSetparam = "drm_i915_gem_context_create_ext_setparam", 56 {
        /// `base.next_extension`.
        next_extension: u64 = 0 as "base",
        /// `param.ctx_id`.
        ctx_id: u32 = 32 as "param",
        /// `param.size`.
        size: u32 = 36 as "param.size",
        /// `param.param`.
        param: u64 = 40 as "param.param",
        /// `param.value`.
        value: u64 = 48 as "param.value",
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_context_param`.
    ContextParam = "drm_i915_gem_context_param", 24 {
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
    /// `struct i915_context_param_engines`'s header; `engines[]` follows.
    ContextParamEngines = "i915_context_param_engines", 8 {
        /// `extensions`.
        extensions: u64 = 0,
    }
);

uapi_struct!(
    /// `struct drm_i915_gem_vm_control`.
    VmControl = "drm_i915_gem_vm_control", 16 {
        /// `extensions`.
        extensions: u64 = 0,
        /// `flags`.
        flags: u32 = 8,
        /// `vm_id`, out on create.
        vm_id: u32 = 12,
    }
);

uapi_struct!(
    /// `struct drm_i915_reset_stats`.
    ResetStats = "drm_i915_reset_stats", 24 {
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
    /// `struct drm_i915_reg_read`.
    RegRead = "drm_i915_reg_read", 16 {
        /// `offset`, with `I915_REG_READ_8B_WA` in bit 0.
        offset: u64 = 0,
        /// `val`, out.
        val: u64 = 8,
    }
);

uapi_struct!(
    /// `struct dma_buf_export_sync_file` (and `dma_buf_import_sync_file`,
    /// the same layout).
    DmaBufSyncFile = "dma_buf_export_sync_file", 8 {
        /// `flags`: `DMA_BUF_SYNC_READ`, `DMA_BUF_SYNC_WRITE`.
        flags: u32 = 0,
        /// `fd`.
        fd: i32 = 4,
    }
);

/// Every structure above, for the probe's test.
pub const LAYOUTS: &[Layout] = &[
    Version::LAYOUT,
    GetCap::LAYOUT,
    GemClose::LAYOUT,
    PrimeHandle::LAYOUT,
    SyncobjCreate::LAYOUT,
    SyncobjDestroy::LAYOUT,
    SyncobjHandle::LAYOUT,
    SyncobjWait::LAYOUT,
    SyncobjTimelineWait::LAYOUT,
    SyncobjArray::LAYOUT,
    SyncobjTimelineArray::LAYOUT,
    SyncobjTransfer::LAYOUT,
    GetParam::LAYOUT,
    Query::LAYOUT,
    QueryItem::LAYOUT,
    TopologyInfo::LAYOUT,
    EngineClassInstance::LAYOUT,
    EngineInfo::LAYOUT,
    QueryEngineInfo::LAYOUT,
    MemoryClassInstance::LAYOUT,
    MemoryRegionInfo::LAYOUT,
    QueryMemoryRegions::LAYOUT,
    UserExtension::LAYOUT,
    GemCreate::LAYOUT,
    GemCreateExt::LAYOUT,
    CreateExtMemoryRegions::LAYOUT,
    CreateExtSetPat::LAYOUT,
    GemMmapOffset::LAYOUT,
    ExecObject2::LAYOUT,
    Execbuffer2::LAYOUT,
    ExecFence::LAYOUT,
    ExecbufferExtTimelineFences::LAYOUT,
    GemWait::LAYOUT,
    GemBusy::LAYOUT,
    SetTiling::LAYOUT,
    GetTiling::LAYOUT,
    Caching::LAYOUT,
    SetDomain::LAYOUT,
    Madvise::LAYOUT,
    GetAperture::LAYOUT,
    ContextCreateExt::LAYOUT,
    ContextCreateExtSetparam::LAYOUT,
    ContextParam::LAYOUT,
    ContextParamEngines::LAYOUT,
    VmControl::LAYOUT,
    ResetStats::LAYOUT,
    RegRead::LAYOUT,
    DmaBufSyncFile::LAYOUT,
];

/// The flexible array members that follow a header above, with their
/// offsets: where the front end writes or reads the array.
pub const FLEXIBLE: &[(&str, &str, usize)] = &[
    ("drm_i915_query_topology_info", "data", TopologyInfo::SIZE),
    (
        "drm_i915_query_engine_info",
        "engines",
        QueryEngineInfo::SIZE,
    ),
    (
        "drm_i915_query_memory_regions",
        "regions",
        QueryMemoryRegions::SIZE,
    ),
    (
        "i915_context_param_engines",
        "engines",
        ContextParamEngines::SIZE,
    ),
];

/// The probe's layouts this module does not model, because the front end
/// answers their ioctls without reading them, or reads them as another
/// structure, each with why.
pub const UNMODELLED: &[(&str, &str)] = &[
    ("drm_set_client_cap", "read as drm_get_cap, the same layout"),
    (
        "drm_i915_gem_create_ext_protected_content",
        "refused: no PXP",
    ),
    ("drm_i915_gem_mmap", "legacy, ENODEV"),
    ("drm_i915_gem_pread", "not served"),
    ("drm_i915_gem_pwrite", "not served"),
    ("drm_i915_gem_sw_finish", "a handle alone"),
    ("drm_i915_gem_userptr", "ENODEV until K7"),
    (
        "drm_i915_gem_context_create",
        "ContextCreateExt's first eight bytes",
    ),
    ("drm_i915_gem_context_destroy", "a context id alone"),
    (
        "dma_buf_import_sync_file",
        "the kernel's; the same layout as DmaBufSyncFile",
    ),
];

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

/// The common DRM ioctls' numbers within `'d'` (`DRM_IOCTL_NR`).
pub mod core_nr {
    /// `DRM_IOCTL_VERSION`.
    pub const VERSION: u8 = 0x00;
    /// `DRM_IOCTL_GEM_CLOSE`.
    pub const GEM_CLOSE: u8 = 0x09;
    /// `DRM_IOCTL_GEM_FLINK`.
    pub const GEM_FLINK: u8 = 0x0a;
    /// `DRM_IOCTL_GEM_OPEN`.
    pub const GEM_OPEN: u8 = 0x0b;
    /// `DRM_IOCTL_GET_CAP`.
    pub const GET_CAP: u8 = 0x0c;
    /// `DRM_IOCTL_SET_CLIENT_CAP`.
    pub const SET_CLIENT_CAP: u8 = 0x0d;
    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD`.
    pub const PRIME_HANDLE_TO_FD: u8 = 0x2d;
    /// `DRM_IOCTL_PRIME_FD_TO_HANDLE`.
    pub const PRIME_FD_TO_HANDLE: u8 = 0x2e;
    /// `DRM_IOCTL_SYNCOBJ_CREATE`.
    pub const SYNCOBJ_CREATE: u8 = 0xbf;
    /// `DRM_IOCTL_SYNCOBJ_DESTROY`.
    pub const SYNCOBJ_DESTROY: u8 = 0xc0;
    /// `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD`.
    pub const SYNCOBJ_HANDLE_TO_FD: u8 = 0xc1;
    /// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE`.
    pub const SYNCOBJ_FD_TO_HANDLE: u8 = 0xc2;
    /// `DRM_IOCTL_SYNCOBJ_WAIT`.
    pub const SYNCOBJ_WAIT: u8 = 0xc3;
    /// `DRM_IOCTL_SYNCOBJ_RESET`.
    pub const SYNCOBJ_RESET: u8 = 0xc4;
    /// `DRM_IOCTL_SYNCOBJ_SIGNAL`.
    pub const SYNCOBJ_SIGNAL: u8 = 0xc5;
    /// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`.
    pub const SYNCOBJ_TIMELINE_WAIT: u8 = 0xca;
    /// `DRM_IOCTL_SYNCOBJ_QUERY`.
    pub const SYNCOBJ_QUERY: u8 = 0xcb;
    /// `DRM_IOCTL_SYNCOBJ_TRANSFER`.
    pub const SYNCOBJ_TRANSFER: u8 = 0xcc;
    /// `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`.
    pub const SYNCOBJ_TIMELINE_SIGNAL: u8 = 0xcd;
    /// `DRM_IOCTL_SYNCOBJ_EVENTFD`.
    pub const SYNCOBJ_EVENTFD: u8 = 0xcf;
}

/// The `DRM_I915_*` command numbers, from `DRM_COMMAND_BASE`.
pub mod nr {
    /// `DRM_I915_GETPARAM`.
    pub const GETPARAM: u8 = 0x06;
    /// `DRM_I915_GEM_BUSY`.
    pub const GEM_BUSY: u8 = 0x17;
    /// `DRM_I915_GEM_THROTTLE`.
    pub const GEM_THROTTLE: u8 = 0x18;
    /// `DRM_I915_GEM_CREATE`.
    pub const GEM_CREATE: u8 = 0x1b;
    /// `DRM_I915_GEM_PREAD`.
    pub const GEM_PREAD: u8 = 0x1c;
    /// `DRM_I915_GEM_PWRITE`.
    pub const GEM_PWRITE: u8 = 0x1d;
    /// `DRM_I915_GEM_MMAP`.
    pub const GEM_MMAP: u8 = 0x1e;
    /// `DRM_I915_GEM_SET_DOMAIN`.
    pub const GEM_SET_DOMAIN: u8 = 0x1f;
    /// `DRM_I915_GEM_SW_FINISH`.
    pub const GEM_SW_FINISH: u8 = 0x20;
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
    /// `DRM_I915_GEM_GET_CACHING`.
    pub const GEM_GET_CACHING: u8 = 0x30;
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
    /// `DRM_I915_PERF_OPEN`.
    pub const PERF_OPEN: u8 = 0x36;
    /// `DRM_I915_PERF_ADD_CONFIG`.
    pub const PERF_ADD_CONFIG: u8 = 0x37;
    /// `DRM_I915_PERF_REMOVE_CONFIG`.
    pub const PERF_REMOVE_CONFIG: u8 = 0x38;
    /// `DRM_I915_QUERY`.
    pub const QUERY: u8 = 0x39;
    /// `DRM_I915_GEM_VM_CREATE`.
    pub const GEM_VM_CREATE: u8 = 0x3a;
    /// `DRM_I915_GEM_VM_DESTROY`.
    pub const GEM_VM_DESTROY: u8 = 0x3b;
    /// `DRM_I915_GEM_CREATE_EXT`.
    pub const GEM_CREATE_EXT: u8 = 0x3c;
}

/// The full ioctl numbers, as `DRM_IOCTL_*` and `DRM_IOCTL_I915_*`.
pub mod ioctl {
    use super::{
        ContextCreateExt, ContextParam, Execbuffer2, GemBusy, GemCreate, GemCreateExt,
        GemMmapOffset, GemWait, GetParam, Query, RegRead, ResetStats, VmControl, drm_ior, drm_iow,
        drm_iowr, nr,
    };

    /// `DRM_IOCTL_VERSION`.
    pub const VERSION: u32 = 0xc040_6400;
    /// `DRM_IOCTL_GET_CAP`.
    pub const GET_CAP: u32 = 0xc010_640c;
    /// `DRM_IOCTL_SET_CLIENT_CAP`.
    pub const SET_CLIENT_CAP: u32 = 0x4010_640d;
    /// `DRM_IOCTL_GEM_CLOSE`.
    pub const GEM_CLOSE: u32 = 0x4008_6409;
    /// `DRM_IOCTL_GEM_FLINK`.
    pub const GEM_FLINK: u32 = 0xc008_640a;
    /// `DRM_IOCTL_GEM_OPEN`.
    pub const GEM_OPEN: u32 = 0xc010_640b;
    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD`.
    pub const PRIME_HANDLE_TO_FD: u32 = 0xc00c_642d;
    /// `DRM_IOCTL_PRIME_FD_TO_HANDLE`.
    pub const PRIME_FD_TO_HANDLE: u32 = 0xc00c_642e;
    /// `DRM_IOCTL_SYNCOBJ_CREATE`.
    pub const SYNCOBJ_CREATE: u32 = 0xc008_64bf;
    /// `DRM_IOCTL_SYNCOBJ_DESTROY`.
    pub const SYNCOBJ_DESTROY: u32 = 0xc008_64c0;
    /// `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD`.
    pub const SYNCOBJ_HANDLE_TO_FD: u32 = 0xc018_64c1;
    /// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE`.
    pub const SYNCOBJ_FD_TO_HANDLE: u32 = 0xc018_64c2;
    /// `DRM_IOCTL_SYNCOBJ_WAIT`.
    pub const SYNCOBJ_WAIT: u32 = 0xc028_64c3;
    /// `DRM_IOCTL_SYNCOBJ_RESET`.
    pub const SYNCOBJ_RESET: u32 = 0xc010_64c4;
    /// `DRM_IOCTL_SYNCOBJ_SIGNAL`.
    pub const SYNCOBJ_SIGNAL: u32 = 0xc010_64c5;
    /// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`.
    pub const SYNCOBJ_TIMELINE_WAIT: u32 = 0xc030_64ca;
    /// `DRM_IOCTL_SYNCOBJ_QUERY`.
    pub const SYNCOBJ_QUERY: u32 = 0xc018_64cb;
    /// `DRM_IOCTL_SYNCOBJ_TRANSFER`.
    pub const SYNCOBJ_TRANSFER: u32 = 0xc020_64cc;
    /// `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`.
    pub const SYNCOBJ_TIMELINE_SIGNAL: u32 = 0xc018_64cd;
    /// `DRM_IOCTL_SYNCOBJ_EVENTFD`.
    pub const SYNCOBJ_EVENTFD: u32 = 0xc018_64cf;

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
    /// `DRM_IOCTL_I915_GEM_MMAP`.
    pub const GEM_MMAP: u32 = drm_iowr(nr::GEM_MMAP, 40);
    /// `DRM_IOCTL_I915_GEM_PREAD`.
    pub const GEM_PREAD: u32 = drm_iow(nr::GEM_PREAD, 32);
    /// `DRM_IOCTL_I915_GEM_PWRITE`.
    pub const GEM_PWRITE: u32 = drm_iow(nr::GEM_PWRITE, 32);
    /// `DRM_IOCTL_I915_GEM_EXECBUFFER2`.
    pub const GEM_EXECBUFFER2: u32 = drm_iow(nr::GEM_EXECBUFFER2, Execbuffer2::SIZE);
    /// `DRM_IOCTL_I915_GEM_EXECBUFFER2_WR`.
    pub const GEM_EXECBUFFER2_WR: u32 = drm_iowr(nr::GEM_EXECBUFFER2, Execbuffer2::SIZE);
    /// `DRM_IOCTL_I915_GEM_WAIT`.
    pub const GEM_WAIT: u32 = drm_iowr(nr::GEM_WAIT, GemWait::SIZE);
    /// `DRM_IOCTL_I915_GEM_BUSY`.
    pub const GEM_BUSY: u32 = drm_iowr(nr::GEM_BUSY, GemBusy::SIZE);
    /// `DRM_IOCTL_I915_GEM_THROTTLE`: `DRM_IO`, no argument.
    pub const GEM_THROTTLE: u32 = super::ioc(
        0,
        super::DRM_IOCTL_BASE,
        super::DRM_COMMAND_BASE + nr::GEM_THROTTLE,
        0,
    );
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
    /// `DRM_IOCTL_I915_GEM_SW_FINISH`.
    pub const GEM_SW_FINISH: u32 = drm_iow(nr::GEM_SW_FINISH, 4);
    /// `DRM_IOCTL_I915_GEM_SET_CACHING`.
    pub const GEM_SET_CACHING: u32 = drm_iow(nr::GEM_SET_CACHING, 8);
    /// `DRM_IOCTL_I915_GEM_GET_CACHING`.
    pub const GEM_GET_CACHING: u32 = drm_iowr(nr::GEM_GET_CACHING, 8);
    /// `DRM_IOCTL_I915_GEM_MADVISE`.
    pub const GEM_MADVISE: u32 = drm_iowr(nr::GEM_MADVISE, 12);
    /// `DRM_IOCTL_I915_GEM_USERPTR`.
    pub const GEM_USERPTR: u32 = drm_iowr(nr::GEM_USERPTR, 24);
    /// `DRM_IOCTL_I915_GEM_GET_APERTURE`.
    pub const GEM_GET_APERTURE: u32 = drm_ior(nr::GEM_GET_APERTURE, 16);
    /// `DRM_IOCTL_I915_PERF_OPEN`.
    pub const PERF_OPEN: u32 = drm_iow(nr::PERF_OPEN, 16);

    /// `DMA_BUF_IOCTL_SYNC`.
    pub const DMA_BUF_SYNC: u32 = 0x4008_6200;
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`.
    pub const DMA_BUF_EXPORT_SYNC_FILE: u32 = 0xc008_6202;
    /// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`.
    pub const DMA_BUF_IMPORT_SYNC_FILE: u32 = 0x4008_6203;
}

/// The constants of the common ioctls (drm.h).
pub mod drm {
    /// `DRM_CAP_PRIME`.
    pub const CAP_PRIME: u64 = 5;
    /// `DRM_PRIME_CAP_IMPORT`.
    pub const PRIME_CAP_IMPORT: u64 = 1;
    /// `DRM_PRIME_CAP_EXPORT`.
    pub const PRIME_CAP_EXPORT: u64 = 2;
    /// `DRM_CAP_TIMESTAMP_MONOTONIC`.
    pub const CAP_TIMESTAMP_MONOTONIC: u64 = 6;
    /// `DRM_CAP_SYNCOBJ`.
    pub const CAP_SYNCOBJ: u64 = 19;
    /// `DRM_CAP_SYNCOBJ_TIMELINE`.
    pub const CAP_SYNCOBJ_TIMELINE: u64 = 20;
    /// `DRM_CLOEXEC`, `O_CLOEXEC`.
    pub const CLOEXEC: u32 = 0o2_000_000;
    /// `DRM_RDWR`, `O_RDWR`.
    pub const RDWR: u32 = 2;
    /// `DRM_SYNCOBJ_CREATE_SIGNALED`.
    pub const SYNCOBJ_CREATE_SIGNALED: u32 = 1;
    /// `DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`.
    pub const SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1;
    /// `DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE`.
    pub const SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE: u32 = 2;
    /// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`.
    pub const SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1;
    /// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE`.
    pub const SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE: u32 = 2;
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL`.
    pub const SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1;
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT`.
    pub const SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 2;
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`.
    pub const SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 4;
    /// `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE`.
    pub const SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE: u32 = 8;
    /// `DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED`.
    pub const SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED: u32 = 1;
}

/// `I915_PARAM_*`.
#[allow(
    missing_docs,
    reason = "each constant is its C name, and VALUES pins it against the probe"
)]
pub mod param {
    pub const IRQ_ACTIVE: i32 = 1;
    pub const ALLOW_BATCHBUFFER: i32 = 2;
    pub const LAST_DISPATCH: i32 = 3;
    pub const CHIPSET_ID: i32 = 4;
    pub const HAS_GEM: i32 = 5;
    pub const NUM_FENCES_AVAIL: i32 = 6;
    pub const HAS_OVERLAY: i32 = 7;
    pub const HAS_PAGEFLIPPING: i32 = 8;
    pub const HAS_EXECBUF2: i32 = 9;
    pub const HAS_BSD: i32 = 10;
    pub const HAS_BLT: i32 = 11;
    pub const HAS_RELAXED_FENCING: i32 = 12;
    pub const HAS_COHERENT_RINGS: i32 = 13;
    pub const HAS_EXEC_CONSTANTS: i32 = 14;
    pub const HAS_RELAXED_DELTA: i32 = 15;
    pub const HAS_GEN7_SOL_RESET: i32 = 16;
    pub const HAS_LLC: i32 = 17;
    pub const HAS_ALIASING_PPGTT: i32 = 18;
    pub const HAS_WAIT_TIMEOUT: i32 = 19;
    pub const HAS_SEMAPHORES: i32 = 20;
    pub const HAS_PRIME_VMAP_FLUSH: i32 = 21;
    pub const HAS_VEBOX: i32 = 22;
    pub const HAS_SECURE_BATCHES: i32 = 23;
    pub const HAS_PINNED_BATCHES: i32 = 24;
    pub const HAS_EXEC_NO_RELOC: i32 = 25;
    pub const HAS_EXEC_HANDLE_LUT: i32 = 26;
    pub const HAS_WT: i32 = 27;
    pub const CMD_PARSER_VERSION: i32 = 28;
    pub const HAS_COHERENT_PHYS_GTT: i32 = 29;
    pub const MMAP_VERSION: i32 = 30;
    pub const HAS_BSD2: i32 = 31;
    pub const REVISION: i32 = 32;
    pub const SUBSLICE_TOTAL: i32 = 33;
    pub const EU_TOTAL: i32 = 34;
    pub const HAS_GPU_RESET: i32 = 35;
    pub const HAS_RESOURCE_STREAMER: i32 = 36;
    pub const HAS_EXEC_SOFTPIN: i32 = 37;
    pub const HAS_POOLED_EU: i32 = 38;
    pub const MIN_EU_IN_POOL: i32 = 39;
    pub const MMAP_GTT_VERSION: i32 = 40;
    pub const HAS_SCHEDULER: i32 = 41;
    pub const HUC_STATUS: i32 = 42;
    pub const HAS_EXEC_ASYNC: i32 = 43;
    pub const HAS_EXEC_FENCE: i32 = 44;
    pub const HAS_EXEC_CAPTURE: i32 = 45;
    pub const SLICE_MASK: i32 = 46;
    pub const SUBSLICE_MASK: i32 = 47;
    pub const HAS_EXEC_BATCH_FIRST: i32 = 48;
    pub const HAS_EXEC_FENCE_ARRAY: i32 = 49;
    pub const HAS_CONTEXT_ISOLATION: i32 = 50;
    pub const CS_TIMESTAMP_FREQUENCY: i32 = 51;
    pub const MMAP_GTT_COHERENT: i32 = 52;
    pub const HAS_EXEC_SUBMIT_FENCE: i32 = 53;
    pub const PERF_REVISION: i32 = 54;
    pub const HAS_EXEC_TIMELINE_FENCES: i32 = 55;
    pub const HAS_USERPTR_PROBE: i32 = 56;
    pub const OA_TIMESTAMP_FREQUENCY: i32 = 57;
    pub const PXP_STATUS: i32 = 58;
    pub const HAS_CONTEXT_FREQ_HINT: i32 = 59;
}

/// `DRM_I915_QUERY_*`, the engine classes and the memory classes.
pub mod query {
    /// `DRM_I915_QUERY_TOPOLOGY_INFO`.
    pub const TOPOLOGY_INFO: u64 = 1;
    /// `DRM_I915_QUERY_ENGINE_INFO`.
    pub const ENGINE_INFO: u64 = 2;
    /// `DRM_I915_QUERY_PERF_CONFIG`.
    pub const PERF_CONFIG: u64 = 3;
    /// `DRM_I915_QUERY_MEMORY_REGIONS`.
    pub const MEMORY_REGIONS: u64 = 4;
    /// `DRM_I915_QUERY_HWCONFIG_BLOB`.
    pub const HWCONFIG_BLOB: u64 = 5;
    /// `DRM_I915_QUERY_GEOMETRY_SUBSLICES`.
    pub const GEOMETRY_SUBSLICES: u64 = 6;
    /// `DRM_I915_QUERY_GUC_SUBMISSION_VERSION`.
    pub const GUC_SUBMISSION_VERSION: u64 = 7;
    /// `I915_ENGINE_CLASS_RENDER`.
    pub const ENGINE_CLASS_RENDER: u16 = 0;
    /// `I915_ENGINE_CLASS_COPY`.
    pub const ENGINE_CLASS_COPY: u16 = 1;
    /// `I915_ENGINE_CLASS_VIDEO`.
    pub const ENGINE_CLASS_VIDEO: u16 = 2;
    /// `I915_ENGINE_CLASS_VIDEO_ENHANCE`.
    pub const ENGINE_CLASS_VIDEO_ENHANCE: u16 = 3;
    /// `I915_ENGINE_CLASS_COMPUTE`.
    pub const ENGINE_CLASS_COMPUTE: u16 = 4;
    /// `I915_ENGINE_CLASS_INVALID`, as the `u16` a structure carries.
    pub const ENGINE_CLASS_INVALID: u16 = 0xffff;
    /// `I915_ENGINE_CLASS_INVALID_NONE`.
    pub const ENGINE_CLASS_INVALID_NONE: u16 = 0xffff;
    /// `I915_ENGINE_CLASS_INVALID_VIRTUAL`.
    pub const ENGINE_CLASS_INVALID_VIRTUAL: u16 = 0xfffe;
    /// `I915_ENGINE_INFO_HAS_LOGICAL_INSTANCE`.
    pub const ENGINE_INFO_HAS_LOGICAL_INSTANCE: u64 = 1;
    /// `I915_MEMORY_CLASS_SYSTEM`.
    pub const MEMORY_CLASS_SYSTEM: u16 = 0;
    /// `I915_MEMORY_CLASS_DEVICE`.
    pub const MEMORY_CLASS_DEVICE: u16 = 1;
}

/// GEM objects' constants.
#[allow(
    missing_docs,
    reason = "each constant is its C name, and VALUES pins it against the probe"
)]
pub mod gem {
    pub const CREATE_EXT_MEMORY_REGIONS: u32 = 0;
    pub const CREATE_EXT_PROTECTED_CONTENT: u32 = 1;
    pub const CREATE_EXT_SET_PAT: u32 = 2;
    pub const CREATE_EXT_FLAG_NEEDS_CPU_ACCESS: u32 = 1;
    pub const MMAP_WC: u64 = 1;
    pub const MMAP_OFFSET_GTT: u64 = 0;
    pub const MMAP_OFFSET_WC: u64 = 1;
    pub const MMAP_OFFSET_WB: u64 = 2;
    pub const MMAP_OFFSET_UC: u64 = 3;
    pub const MMAP_OFFSET_FIXED: u64 = 4;
    pub const DOMAIN_CPU: u32 = 1;
    pub const DOMAIN_RENDER: u32 = 2;
    pub const DOMAIN_GTT: u32 = 0x40;
    pub const DOMAIN_WC: u32 = 0x80;
    pub const CACHING_NONE: u32 = 0;
    pub const CACHING_CACHED: u32 = 1;
    pub const CACHING_DISPLAY: u32 = 2;
    pub const TILING_NONE: u32 = 0;
    pub const TILING_X: u32 = 1;
    pub const TILING_Y: u32 = 2;
    pub const TILING_LAST: u32 = 2;
    pub const BIT_6_SWIZZLE_NONE: u32 = 0;
    pub const MADV_WILLNEED: u32 = 0;
    pub const MADV_DONTNEED: u32 = 1;
    pub const USERPTR_READ_ONLY: u32 = 1;
    pub const USERPTR_PROBE: u32 = 2;
    pub const USERPTR_UNSYNCHRONIZED: u32 = 0x8000_0000;
    pub const REG_READ_8B_WA: u64 = 1;
}

/// `EXECBUFFER2`'s constants.
#[allow(
    missing_docs,
    reason = "each constant is its C name, and VALUES pins it against the probe"
)]
pub mod exec {
    pub const OBJECT_NEEDS_FENCE: u64 = 1 << 0;
    pub const OBJECT_NEEDS_GTT: u64 = 1 << 1;
    pub const OBJECT_WRITE: u64 = 1 << 2;
    pub const OBJECT_SUPPORTS_48B_ADDRESS: u64 = 1 << 3;
    pub const OBJECT_PINNED: u64 = 1 << 4;
    pub const OBJECT_PAD_TO_SIZE: u64 = 1 << 5;
    pub const OBJECT_ASYNC: u64 = 1 << 6;
    pub const OBJECT_CAPTURE: u64 = 1 << 7;
    /// `__EXEC_OBJECT_UNKNOWN_FLAGS`: `-(EXEC_OBJECT_CAPTURE << 1)`.
    pub const OBJECT_UNKNOWN_FLAGS: u64 = (OBJECT_CAPTURE << 1).wrapping_neg();
    pub const RING_MASK: u64 = 0x3f;
    pub const DEFAULT: u64 = 0;
    pub const RENDER: u64 = 1;
    pub const BSD: u64 = 2;
    pub const BLT: u64 = 3;
    pub const VEBOX: u64 = 4;
    pub const CONSTANTS_MASK: u64 = 3 << 6;
    pub const GEN7_SOL_RESET: u64 = 1 << 8;
    pub const SECURE: u64 = 1 << 9;
    pub const IS_PINNED: u64 = 1 << 10;
    pub const NO_RELOC: u64 = 1 << 11;
    pub const HANDLE_LUT: u64 = 1 << 12;
    pub const BSD_MASK: u64 = 3 << 13;
    pub const RESOURCE_STREAMER: u64 = 1 << 15;
    pub const FENCE_IN: u64 = 1 << 16;
    pub const FENCE_OUT: u64 = 1 << 17;
    pub const BATCH_FIRST: u64 = 1 << 18;
    pub const FENCE_ARRAY: u64 = 1 << 19;
    pub const FENCE_SUBMIT: u64 = 1 << 20;
    pub const USE_EXTENSIONS: u64 = 1 << 21;
    /// `__I915_EXEC_UNKNOWN_FLAGS`: `-(I915_EXEC_USE_EXTENSIONS << 1)`.
    pub const UNKNOWN_FLAGS: u64 = (USE_EXTENSIONS << 1).wrapping_neg();
    pub const CONTEXT_ID_MASK: u64 = 0xffff_ffff;
    pub const FENCE_WAIT: u32 = 1 << 0;
    pub const FENCE_SIGNAL: u32 = 1 << 1;
    /// `__I915_EXEC_FENCE_UNKNOWN_FLAGS`: `-(I915_EXEC_FENCE_SIGNAL << 1)`.
    pub const FENCE_UNKNOWN_FLAGS: u32 = (FENCE_SIGNAL << 1).wrapping_neg();
    pub const EXT_TIMELINE_FENCES: u32 = 0;
}

/// Contexts' constants.
#[allow(
    missing_docs,
    reason = "each constant is its C name, and VALUES pins it against the probe"
)]
pub mod context {
    pub const CREATE_FLAGS_USE_EXTENSIONS: u32 = 1 << 0;
    pub const CREATE_FLAGS_SINGLE_TIMELINE: u32 = 1 << 1;
    /// `I915_CONTEXT_CREATE_FLAGS_UNKNOWN`: `-(SINGLE_TIMELINE << 1)`.
    pub const CREATE_FLAGS_UNKNOWN: u32 = (CREATE_FLAGS_SINGLE_TIMELINE << 1).wrapping_neg();
    pub const CREATE_EXT_SETPARAM: u32 = 0;
    pub const CREATE_EXT_CLONE: u32 = 1;
    pub const PARAM_BAN_PERIOD: u64 = 1;
    pub const PARAM_NO_ZEROMAP: u64 = 2;
    pub const PARAM_GTT_SIZE: u64 = 3;
    pub const PARAM_NO_ERROR_CAPTURE: u64 = 4;
    pub const PARAM_BANNABLE: u64 = 5;
    pub const PARAM_PRIORITY: u64 = 6;
    pub const PARAM_SSEU: u64 = 7;
    pub const PARAM_RECOVERABLE: u64 = 8;
    pub const PARAM_VM: u64 = 9;
    pub const PARAM_ENGINES: u64 = 10;
    pub const PARAM_PERSISTENCE: u64 = 11;
    pub const PARAM_RINGSIZE: u64 = 12;
    pub const PARAM_PROTECTED_CONTENT: u64 = 13;
    pub const PARAM_LOW_LATENCY: u64 = 14;
    pub const PARAM_CONTEXT_IMAGE: u64 = 15;
    pub const MAX_USER_PRIORITY: i64 = 1023;
    pub const DEFAULT_PRIORITY: i64 = 0;
    pub const MIN_USER_PRIORITY: i64 = -1023;
    pub const ENGINES_EXT_LOAD_BALANCE: u32 = 0;
    pub const ENGINES_EXT_BOND: u32 = 1;
    pub const ENGINES_EXT_PARALLEL_SUBMIT: u32 = 2;
}

/// Every constant above, by its C name, for the probe's test: each line of
/// `probe/i915-64.txt` that is not a layout must be here with its value, and
/// each entry here must be in the file. Negative and `unsigned int` values
/// are as the probe prints them, widened to 64 bits by sign.
pub const VALUES: &[(&str, u64)] = &[
    ("DRM_IOCTL_VERSION", ioctl::VERSION as u64),
    ("DRM_IOCTL_GET_CAP", ioctl::GET_CAP as u64),
    ("DRM_IOCTL_SET_CLIENT_CAP", ioctl::SET_CLIENT_CAP as u64),
    ("DRM_IOCTL_GEM_CLOSE", ioctl::GEM_CLOSE as u64),
    ("DRM_IOCTL_GEM_FLINK", ioctl::GEM_FLINK as u64),
    ("DRM_IOCTL_GEM_OPEN", ioctl::GEM_OPEN as u64),
    (
        "DRM_IOCTL_PRIME_HANDLE_TO_FD",
        ioctl::PRIME_HANDLE_TO_FD as u64,
    ),
    (
        "DRM_IOCTL_PRIME_FD_TO_HANDLE",
        ioctl::PRIME_FD_TO_HANDLE as u64,
    ),
    ("DRM_IOCTL_SYNCOBJ_CREATE", ioctl::SYNCOBJ_CREATE as u64),
    ("DRM_IOCTL_SYNCOBJ_DESTROY", ioctl::SYNCOBJ_DESTROY as u64),
    (
        "DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD",
        ioctl::SYNCOBJ_HANDLE_TO_FD as u64,
    ),
    (
        "DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE",
        ioctl::SYNCOBJ_FD_TO_HANDLE as u64,
    ),
    ("DRM_IOCTL_SYNCOBJ_WAIT", ioctl::SYNCOBJ_WAIT as u64),
    ("DRM_IOCTL_SYNCOBJ_RESET", ioctl::SYNCOBJ_RESET as u64),
    ("DRM_IOCTL_SYNCOBJ_SIGNAL", ioctl::SYNCOBJ_SIGNAL as u64),
    (
        "DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT",
        ioctl::SYNCOBJ_TIMELINE_WAIT as u64,
    ),
    ("DRM_IOCTL_SYNCOBJ_QUERY", ioctl::SYNCOBJ_QUERY as u64),
    ("DRM_IOCTL_SYNCOBJ_TRANSFER", ioctl::SYNCOBJ_TRANSFER as u64),
    (
        "DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL",
        ioctl::SYNCOBJ_TIMELINE_SIGNAL as u64,
    ),
    ("DRM_IOCTL_SYNCOBJ_EVENTFD", ioctl::SYNCOBJ_EVENTFD as u64),
    ("DRM_IOCTL_I915_GETPARAM", ioctl::GETPARAM as u64),
    (
        "DRM_IOCTL_I915_GEM_EXECBUFFER2",
        ioctl::GEM_EXECBUFFER2 as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_EXECBUFFER2_WR",
        ioctl::GEM_EXECBUFFER2_WR as u64,
    ),
    ("DRM_IOCTL_I915_GEM_BUSY", ioctl::GEM_BUSY as u64),
    (
        "DRM_IOCTL_I915_GEM_SET_CACHING",
        ioctl::GEM_SET_CACHING as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_GET_CACHING",
        ioctl::GEM_GET_CACHING as u64,
    ),
    ("DRM_IOCTL_I915_GEM_THROTTLE", ioctl::GEM_THROTTLE as u64),
    ("DRM_IOCTL_I915_GEM_CREATE", ioctl::GEM_CREATE as u64),
    (
        "DRM_IOCTL_I915_GEM_CREATE_EXT",
        ioctl::GEM_CREATE_EXT as u64,
    ),
    ("DRM_IOCTL_I915_GEM_PREAD", ioctl::GEM_PREAD as u64),
    ("DRM_IOCTL_I915_GEM_PWRITE", ioctl::GEM_PWRITE as u64),
    ("DRM_IOCTL_I915_GEM_MMAP", ioctl::GEM_MMAP as u64),
    (
        "DRM_IOCTL_I915_GEM_MMAP_OFFSET",
        ioctl::GEM_MMAP_OFFSET as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_SET_DOMAIN",
        ioctl::GEM_SET_DOMAIN as u64,
    ),
    ("DRM_IOCTL_I915_GEM_SW_FINISH", ioctl::GEM_SW_FINISH as u64),
    (
        "DRM_IOCTL_I915_GEM_SET_TILING",
        ioctl::GEM_SET_TILING as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_GET_TILING",
        ioctl::GEM_GET_TILING as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_GET_APERTURE",
        ioctl::GEM_GET_APERTURE as u64,
    ),
    ("DRM_IOCTL_I915_GEM_MADVISE", ioctl::GEM_MADVISE as u64),
    ("DRM_IOCTL_I915_GEM_WAIT", ioctl::GEM_WAIT as u64),
    (
        "DRM_IOCTL_I915_GEM_CONTEXT_CREATE",
        ioctl::GEM_CONTEXT_CREATE as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_CONTEXT_CREATE_EXT",
        ioctl::GEM_CONTEXT_CREATE_EXT as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_CONTEXT_DESTROY",
        ioctl::GEM_CONTEXT_DESTROY as u64,
    ),
    ("DRM_IOCTL_I915_REG_READ", ioctl::REG_READ as u64),
    (
        "DRM_IOCTL_I915_GET_RESET_STATS",
        ioctl::GET_RESET_STATS as u64,
    ),
    ("DRM_IOCTL_I915_GEM_USERPTR", ioctl::GEM_USERPTR as u64),
    (
        "DRM_IOCTL_I915_GEM_CONTEXT_GETPARAM",
        ioctl::GEM_CONTEXT_GETPARAM as u64,
    ),
    (
        "DRM_IOCTL_I915_GEM_CONTEXT_SETPARAM",
        ioctl::GEM_CONTEXT_SETPARAM as u64,
    ),
    ("DRM_IOCTL_I915_PERF_OPEN", ioctl::PERF_OPEN as u64),
    ("DRM_IOCTL_I915_QUERY", ioctl::QUERY as u64),
    ("DRM_IOCTL_I915_GEM_VM_CREATE", ioctl::GEM_VM_CREATE as u64),
    (
        "DRM_IOCTL_I915_GEM_VM_DESTROY",
        ioctl::GEM_VM_DESTROY as u64,
    ),
    ("DMA_BUF_IOCTL_SYNC", ioctl::DMA_BUF_SYNC as u64),
    (
        "DMA_BUF_IOCTL_EXPORT_SYNC_FILE",
        ioctl::DMA_BUF_EXPORT_SYNC_FILE as u64,
    ),
    (
        "DMA_BUF_IOCTL_IMPORT_SYNC_FILE",
        ioctl::DMA_BUF_IMPORT_SYNC_FILE as u64,
    ),
    ("DRM_CAP_PRIME", drm::CAP_PRIME),
    ("DRM_PRIME_CAP_IMPORT", drm::PRIME_CAP_IMPORT),
    ("DRM_PRIME_CAP_EXPORT", drm::PRIME_CAP_EXPORT),
    ("DRM_CAP_TIMESTAMP_MONOTONIC", drm::CAP_TIMESTAMP_MONOTONIC),
    ("DRM_CAP_SYNCOBJ", drm::CAP_SYNCOBJ),
    ("DRM_CAP_SYNCOBJ_TIMELINE", drm::CAP_SYNCOBJ_TIMELINE),
    ("DRM_CLOEXEC", drm::CLOEXEC as u64),
    ("DRM_RDWR", drm::RDWR as u64),
    (
        "DRM_SYNCOBJ_CREATE_SIGNALED",
        drm::SYNCOBJ_CREATE_SIGNALED as u64,
    ),
    (
        "DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE",
        drm::SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE as u64,
    ),
    (
        "DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE",
        drm::SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE as u64,
    ),
    (
        "DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE",
        drm::SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE as u64,
    ),
    (
        "DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE",
        drm::SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE as u64,
    ),
    (
        "DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL",
        drm::SYNCOBJ_WAIT_FLAGS_WAIT_ALL as u64,
    ),
    (
        "DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT",
        drm::SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT as u64,
    ),
    (
        "DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE",
        drm::SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE as u64,
    ),
    (
        "DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE",
        drm::SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE as u64,
    ),
    (
        "DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED",
        drm::SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED as u64,
    ),
    ("I915_PARAM_IRQ_ACTIVE", param::IRQ_ACTIVE as u64),
    (
        "I915_PARAM_ALLOW_BATCHBUFFER",
        param::ALLOW_BATCHBUFFER as u64,
    ),
    ("I915_PARAM_LAST_DISPATCH", param::LAST_DISPATCH as u64),
    ("I915_PARAM_CHIPSET_ID", param::CHIPSET_ID as u64),
    ("I915_PARAM_HAS_GEM", param::HAS_GEM as u64),
    (
        "I915_PARAM_NUM_FENCES_AVAIL",
        param::NUM_FENCES_AVAIL as u64,
    ),
    ("I915_PARAM_HAS_OVERLAY", param::HAS_OVERLAY as u64),
    (
        "I915_PARAM_HAS_PAGEFLIPPING",
        param::HAS_PAGEFLIPPING as u64,
    ),
    ("I915_PARAM_HAS_EXECBUF2", param::HAS_EXECBUF2 as u64),
    ("I915_PARAM_HAS_BSD", param::HAS_BSD as u64),
    ("I915_PARAM_HAS_BLT", param::HAS_BLT as u64),
    (
        "I915_PARAM_HAS_RELAXED_FENCING",
        param::HAS_RELAXED_FENCING as u64,
    ),
    (
        "I915_PARAM_HAS_COHERENT_RINGS",
        param::HAS_COHERENT_RINGS as u64,
    ),
    (
        "I915_PARAM_HAS_EXEC_CONSTANTS",
        param::HAS_EXEC_CONSTANTS as u64,
    ),
    (
        "I915_PARAM_HAS_RELAXED_DELTA",
        param::HAS_RELAXED_DELTA as u64,
    ),
    (
        "I915_PARAM_HAS_GEN7_SOL_RESET",
        param::HAS_GEN7_SOL_RESET as u64,
    ),
    ("I915_PARAM_HAS_LLC", param::HAS_LLC as u64),
    (
        "I915_PARAM_HAS_ALIASING_PPGTT",
        param::HAS_ALIASING_PPGTT as u64,
    ),
    (
        "I915_PARAM_HAS_WAIT_TIMEOUT",
        param::HAS_WAIT_TIMEOUT as u64,
    ),
    ("I915_PARAM_HAS_SEMAPHORES", param::HAS_SEMAPHORES as u64),
    (
        "I915_PARAM_HAS_PRIME_VMAP_FLUSH",
        param::HAS_PRIME_VMAP_FLUSH as u64,
    ),
    ("I915_PARAM_HAS_VEBOX", param::HAS_VEBOX as u64),
    (
        "I915_PARAM_HAS_SECURE_BATCHES",
        param::HAS_SECURE_BATCHES as u64,
    ),
    (
        "I915_PARAM_HAS_PINNED_BATCHES",
        param::HAS_PINNED_BATCHES as u64,
    ),
    (
        "I915_PARAM_HAS_EXEC_NO_RELOC",
        param::HAS_EXEC_NO_RELOC as u64,
    ),
    (
        "I915_PARAM_HAS_EXEC_HANDLE_LUT",
        param::HAS_EXEC_HANDLE_LUT as u64,
    ),
    ("I915_PARAM_HAS_WT", param::HAS_WT as u64),
    (
        "I915_PARAM_CMD_PARSER_VERSION",
        param::CMD_PARSER_VERSION as u64,
    ),
    (
        "I915_PARAM_HAS_COHERENT_PHYS_GTT",
        param::HAS_COHERENT_PHYS_GTT as u64,
    ),
    ("I915_PARAM_MMAP_VERSION", param::MMAP_VERSION as u64),
    ("I915_PARAM_HAS_BSD2", param::HAS_BSD2 as u64),
    ("I915_PARAM_REVISION", param::REVISION as u64),
    ("I915_PARAM_SUBSLICE_TOTAL", param::SUBSLICE_TOTAL as u64),
    ("I915_PARAM_EU_TOTAL", param::EU_TOTAL as u64),
    ("I915_PARAM_HAS_GPU_RESET", param::HAS_GPU_RESET as u64),
    (
        "I915_PARAM_HAS_RESOURCE_STREAMER",
        param::HAS_RESOURCE_STREAMER as u64,
    ),
    (
        "I915_PARAM_HAS_EXEC_SOFTPIN",
        param::HAS_EXEC_SOFTPIN as u64,
    ),
    ("I915_PARAM_HAS_POOLED_EU", param::HAS_POOLED_EU as u64),
    ("I915_PARAM_MIN_EU_IN_POOL", param::MIN_EU_IN_POOL as u64),
    (
        "I915_PARAM_MMAP_GTT_VERSION",
        param::MMAP_GTT_VERSION as u64,
    ),
    ("I915_PARAM_HAS_SCHEDULER", param::HAS_SCHEDULER as u64),
    ("I915_PARAM_HUC_STATUS", param::HUC_STATUS as u64),
    ("I915_PARAM_HAS_EXEC_ASYNC", param::HAS_EXEC_ASYNC as u64),
    ("I915_PARAM_HAS_EXEC_FENCE", param::HAS_EXEC_FENCE as u64),
    (
        "I915_PARAM_HAS_EXEC_CAPTURE",
        param::HAS_EXEC_CAPTURE as u64,
    ),
    ("I915_PARAM_SLICE_MASK", param::SLICE_MASK as u64),
    ("I915_PARAM_SUBSLICE_MASK", param::SUBSLICE_MASK as u64),
    (
        "I915_PARAM_HAS_EXEC_BATCH_FIRST",
        param::HAS_EXEC_BATCH_FIRST as u64,
    ),
    (
        "I915_PARAM_HAS_EXEC_FENCE_ARRAY",
        param::HAS_EXEC_FENCE_ARRAY as u64,
    ),
    (
        "I915_PARAM_HAS_CONTEXT_ISOLATION",
        param::HAS_CONTEXT_ISOLATION as u64,
    ),
    (
        "I915_PARAM_CS_TIMESTAMP_FREQUENCY",
        param::CS_TIMESTAMP_FREQUENCY as u64,
    ),
    (
        "I915_PARAM_MMAP_GTT_COHERENT",
        param::MMAP_GTT_COHERENT as u64,
    ),
    (
        "I915_PARAM_HAS_EXEC_SUBMIT_FENCE",
        param::HAS_EXEC_SUBMIT_FENCE as u64,
    ),
    ("I915_PARAM_PERF_REVISION", param::PERF_REVISION as u64),
    (
        "I915_PARAM_HAS_EXEC_TIMELINE_FENCES",
        param::HAS_EXEC_TIMELINE_FENCES as u64,
    ),
    (
        "I915_PARAM_HAS_USERPTR_PROBE",
        param::HAS_USERPTR_PROBE as u64,
    ),
    (
        "I915_PARAM_OA_TIMESTAMP_FREQUENCY",
        param::OA_TIMESTAMP_FREQUENCY as u64,
    ),
    ("I915_PARAM_PXP_STATUS", param::PXP_STATUS as u64),
    (
        "I915_PARAM_HAS_CONTEXT_FREQ_HINT",
        param::HAS_CONTEXT_FREQ_HINT as u64,
    ),
    ("DRM_I915_QUERY_TOPOLOGY_INFO", query::TOPOLOGY_INFO),
    ("DRM_I915_QUERY_ENGINE_INFO", query::ENGINE_INFO),
    ("DRM_I915_QUERY_PERF_CONFIG", query::PERF_CONFIG),
    ("DRM_I915_QUERY_MEMORY_REGIONS", query::MEMORY_REGIONS),
    ("DRM_I915_QUERY_HWCONFIG_BLOB", query::HWCONFIG_BLOB),
    (
        "DRM_I915_QUERY_GEOMETRY_SUBSLICES",
        query::GEOMETRY_SUBSLICES,
    ),
    (
        "DRM_I915_QUERY_GUC_SUBMISSION_VERSION",
        query::GUC_SUBMISSION_VERSION,
    ),
    (
        "I915_ENGINE_CLASS_RENDER",
        query::ENGINE_CLASS_RENDER as u64,
    ),
    ("I915_ENGINE_CLASS_COPY", query::ENGINE_CLASS_COPY as u64),
    ("I915_ENGINE_CLASS_VIDEO", query::ENGINE_CLASS_VIDEO as u64),
    (
        "I915_ENGINE_CLASS_VIDEO_ENHANCE",
        query::ENGINE_CLASS_VIDEO_ENHANCE as u64,
    ),
    (
        "I915_ENGINE_CLASS_COMPUTE",
        query::ENGINE_CLASS_COMPUTE as u64,
    ),
    ("I915_ENGINE_CLASS_INVALID", u64::MAX),
    ("I915_ENGINE_CLASS_INVALID_NONE", u64::MAX),
    ("I915_ENGINE_CLASS_INVALID_VIRTUAL", u64::MAX - 1),
    (
        "I915_ENGINE_INFO_HAS_LOGICAL_INSTANCE",
        query::ENGINE_INFO_HAS_LOGICAL_INSTANCE,
    ),
    (
        "I915_MEMORY_CLASS_SYSTEM",
        query::MEMORY_CLASS_SYSTEM as u64,
    ),
    (
        "I915_MEMORY_CLASS_DEVICE",
        query::MEMORY_CLASS_DEVICE as u64,
    ),
    (
        "I915_GEM_CREATE_EXT_MEMORY_REGIONS",
        gem::CREATE_EXT_MEMORY_REGIONS as u64,
    ),
    (
        "I915_GEM_CREATE_EXT_PROTECTED_CONTENT",
        gem::CREATE_EXT_PROTECTED_CONTENT as u64,
    ),
    (
        "I915_GEM_CREATE_EXT_SET_PAT",
        gem::CREATE_EXT_SET_PAT as u64,
    ),
    (
        "I915_GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS",
        gem::CREATE_EXT_FLAG_NEEDS_CPU_ACCESS as u64,
    ),
    ("I915_MMAP_WC", gem::MMAP_WC),
    ("I915_MMAP_OFFSET_GTT", gem::MMAP_OFFSET_GTT),
    ("I915_MMAP_OFFSET_WC", gem::MMAP_OFFSET_WC),
    ("I915_MMAP_OFFSET_WB", gem::MMAP_OFFSET_WB),
    ("I915_MMAP_OFFSET_UC", gem::MMAP_OFFSET_UC),
    ("I915_MMAP_OFFSET_FIXED", gem::MMAP_OFFSET_FIXED),
    ("I915_GEM_DOMAIN_CPU", gem::DOMAIN_CPU as u64),
    ("I915_GEM_DOMAIN_RENDER", gem::DOMAIN_RENDER as u64),
    ("I915_GEM_DOMAIN_GTT", gem::DOMAIN_GTT as u64),
    ("I915_GEM_DOMAIN_WC", gem::DOMAIN_WC as u64),
    ("I915_CACHING_NONE", gem::CACHING_NONE as u64),
    ("I915_CACHING_CACHED", gem::CACHING_CACHED as u64),
    ("I915_CACHING_DISPLAY", gem::CACHING_DISPLAY as u64),
    ("I915_TILING_NONE", gem::TILING_NONE as u64),
    ("I915_TILING_X", gem::TILING_X as u64),
    ("I915_TILING_Y", gem::TILING_Y as u64),
    ("I915_TILING_LAST", gem::TILING_LAST as u64),
    ("I915_BIT_6_SWIZZLE_NONE", gem::BIT_6_SWIZZLE_NONE as u64),
    ("I915_MADV_WILLNEED", gem::MADV_WILLNEED as u64),
    ("I915_MADV_DONTNEED", gem::MADV_DONTNEED as u64),
    ("I915_USERPTR_READ_ONLY", gem::USERPTR_READ_ONLY as u64),
    ("I915_USERPTR_PROBE", gem::USERPTR_PROBE as u64),
    (
        "I915_USERPTR_UNSYNCHRONIZED",
        gem::USERPTR_UNSYNCHRONIZED as u64,
    ),
    ("I915_REG_READ_8B_WA", gem::REG_READ_8B_WA),
    ("EXEC_OBJECT_NEEDS_FENCE", exec::OBJECT_NEEDS_FENCE),
    ("EXEC_OBJECT_NEEDS_GTT", exec::OBJECT_NEEDS_GTT),
    ("EXEC_OBJECT_WRITE", exec::OBJECT_WRITE),
    (
        "EXEC_OBJECT_SUPPORTS_48B_ADDRESS",
        exec::OBJECT_SUPPORTS_48B_ADDRESS,
    ),
    ("EXEC_OBJECT_PINNED", exec::OBJECT_PINNED),
    ("EXEC_OBJECT_PAD_TO_SIZE", exec::OBJECT_PAD_TO_SIZE),
    ("EXEC_OBJECT_ASYNC", exec::OBJECT_ASYNC),
    ("EXEC_OBJECT_CAPTURE", exec::OBJECT_CAPTURE),
    ("__EXEC_OBJECT_UNKNOWN_FLAGS", exec::OBJECT_UNKNOWN_FLAGS),
    ("I915_EXEC_RING_MASK", exec::RING_MASK),
    ("I915_EXEC_DEFAULT", exec::DEFAULT),
    ("I915_EXEC_RENDER", exec::RENDER),
    ("I915_EXEC_BSD", exec::BSD),
    ("I915_EXEC_BLT", exec::BLT),
    ("I915_EXEC_VEBOX", exec::VEBOX),
    ("I915_EXEC_CONSTANTS_MASK", exec::CONSTANTS_MASK),
    ("I915_EXEC_GEN7_SOL_RESET", exec::GEN7_SOL_RESET),
    ("I915_EXEC_SECURE", exec::SECURE),
    ("I915_EXEC_IS_PINNED", exec::IS_PINNED),
    ("I915_EXEC_NO_RELOC", exec::NO_RELOC),
    ("I915_EXEC_HANDLE_LUT", exec::HANDLE_LUT),
    ("I915_EXEC_BSD_MASK", exec::BSD_MASK),
    ("I915_EXEC_RESOURCE_STREAMER", exec::RESOURCE_STREAMER),
    ("I915_EXEC_FENCE_IN", exec::FENCE_IN),
    ("I915_EXEC_FENCE_OUT", exec::FENCE_OUT),
    ("I915_EXEC_BATCH_FIRST", exec::BATCH_FIRST),
    ("I915_EXEC_FENCE_ARRAY", exec::FENCE_ARRAY),
    ("I915_EXEC_FENCE_SUBMIT", exec::FENCE_SUBMIT),
    ("I915_EXEC_USE_EXTENSIONS", exec::USE_EXTENSIONS),
    ("__I915_EXEC_UNKNOWN_FLAGS", exec::UNKNOWN_FLAGS),
    ("I915_EXEC_CONTEXT_ID_MASK", exec::CONTEXT_ID_MASK),
    ("I915_EXEC_FENCE_WAIT", exec::FENCE_WAIT as u64),
    ("I915_EXEC_FENCE_SIGNAL", exec::FENCE_SIGNAL as u64),
    (
        "__I915_EXEC_FENCE_UNKNOWN_FLAGS",
        exec::FENCE_UNKNOWN_FLAGS as i32 as i64 as u64,
    ),
    (
        "DRM_I915_GEM_EXECBUFFER_EXT_TIMELINE_FENCES",
        exec::EXT_TIMELINE_FENCES as u64,
    ),
    (
        "I915_CONTEXT_CREATE_FLAGS_USE_EXTENSIONS",
        context::CREATE_FLAGS_USE_EXTENSIONS as u64,
    ),
    (
        "I915_CONTEXT_CREATE_FLAGS_SINGLE_TIMELINE",
        context::CREATE_FLAGS_SINGLE_TIMELINE as u64,
    ),
    (
        "I915_CONTEXT_CREATE_FLAGS_UNKNOWN",
        context::CREATE_FLAGS_UNKNOWN as u64,
    ),
    (
        "I915_CONTEXT_CREATE_EXT_SETPARAM",
        context::CREATE_EXT_SETPARAM as u64,
    ),
    (
        "I915_CONTEXT_CREATE_EXT_CLONE",
        context::CREATE_EXT_CLONE as u64,
    ),
    ("I915_CONTEXT_PARAM_BAN_PERIOD", context::PARAM_BAN_PERIOD),
    ("I915_CONTEXT_PARAM_NO_ZEROMAP", context::PARAM_NO_ZEROMAP),
    ("I915_CONTEXT_PARAM_GTT_SIZE", context::PARAM_GTT_SIZE),
    (
        "I915_CONTEXT_PARAM_NO_ERROR_CAPTURE",
        context::PARAM_NO_ERROR_CAPTURE,
    ),
    ("I915_CONTEXT_PARAM_BANNABLE", context::PARAM_BANNABLE),
    ("I915_CONTEXT_PARAM_PRIORITY", context::PARAM_PRIORITY),
    ("I915_CONTEXT_PARAM_SSEU", context::PARAM_SSEU),
    ("I915_CONTEXT_PARAM_RECOVERABLE", context::PARAM_RECOVERABLE),
    ("I915_CONTEXT_PARAM_VM", context::PARAM_VM),
    ("I915_CONTEXT_PARAM_ENGINES", context::PARAM_ENGINES),
    ("I915_CONTEXT_PARAM_PERSISTENCE", context::PARAM_PERSISTENCE),
    ("I915_CONTEXT_PARAM_RINGSIZE", context::PARAM_RINGSIZE),
    (
        "I915_CONTEXT_PARAM_PROTECTED_CONTENT",
        context::PARAM_PROTECTED_CONTENT,
    ),
    ("I915_CONTEXT_PARAM_LOW_LATENCY", context::PARAM_LOW_LATENCY),
    (
        "I915_CONTEXT_PARAM_CONTEXT_IMAGE",
        context::PARAM_CONTEXT_IMAGE,
    ),
    (
        "I915_CONTEXT_MAX_USER_PRIORITY",
        context::MAX_USER_PRIORITY as u64,
    ),
    (
        "I915_CONTEXT_DEFAULT_PRIORITY",
        context::DEFAULT_PRIORITY as u64,
    ),
    (
        "I915_CONTEXT_MIN_USER_PRIORITY",
        context::MIN_USER_PRIORITY as u64,
    ),
    (
        "I915_CONTEXT_ENGINES_EXT_LOAD_BALANCE",
        context::ENGINES_EXT_LOAD_BALANCE as u64,
    ),
    (
        "I915_CONTEXT_ENGINES_EXT_BOND",
        context::ENGINES_EXT_BOND as u64,
    ),
    (
        "I915_CONTEXT_ENGINES_EXT_PARALLEL_SUBMIT",
        context::ENGINES_EXT_PARALLEL_SUBMIT as u64,
    ),
];
