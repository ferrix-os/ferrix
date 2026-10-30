//! GEM objects: `gem/i915_gem_create.c`, `i915_gem_mman.c`,
//! `i915_gem_tiling.c`, `i915_gem_domain.c`, `i915_gem_wait.c`,
//! `i915_gem_busy.c`, `drm_gem.c`'s `GEM_CLOSE`, and `drm_prime.c`.
//!
//! An object is a VMO of the back end's (`Backend::Memory`), whole, never
//! swapped or purged. Its mmaps are answered with the VMO itself
//! (`MapReply::Vmo`), write-back or write-combining by its offset's mode
//! (K6), except that a write-combining mmap is served write-back where
//! [`wc_served_cached`] says the two cannot be told apart. Tiling, caching, domains and advice are recorded and told back: on
//! an LLC part with softpin and no fence registers none of them changes what
//! the GPU or the CPU sees.

use ferrix_linux_abi::errno::Errno;

use super::drm::{Call, put, unserved};
use super::fence::{Hold, WaitKind};
use super::objects::{FenceRef, Handle, Object, Tables};
use super::{Backend, ETIME, Frontend, Host, limits};
use crate::uapi::{
    Caching, CreateExtMemoryRegions, GemBusy, GemClose, GemCreate, GemCreateExt, GemMmapOffset,
    GemWait, GetAperture, GetTiling, Madvise, MemoryClassInstance, PrimeHandle, SetDomain,
    SetTiling, drm as consts, gem, query,
};

/// 4 KiB.
const PAGE: u64 = 4096;

/// `i915_gem_object_size_2big`: more pages than an `int` counts.
const MAX_PAGES: u64 = i32::MAX as u64;

/// i915's PAT index for `I915_CACHE_LLC` on Gen12 (`tgl_pat` index 0, WB),
/// what a new object on an LLC part gets (`i915_gem_object_set_cache_coherency`).
const PAT_WB: u8 = 0;
/// Whether the parts this front end serves have an LLC the GPU snoops
/// (`HAS_LLC`): every Gen12 integrated part (Tiger Lake, Alder Lake, ADL-N)
/// does. `GETPARAM`'s `HAS_LLC` says this.
pub const HAS_LLC: bool = true;

/// Whether anything can hand an object of this driver to a display plane.
/// Nothing can: the node is render-only (no KMS, no card node), the buffer
/// yserver's DRI3 hands hyprix as a linux-dmabuf is read by hyprix's CPU
/// composite through the LLC and never scanned out (`docs/INTEL-GPU.md`
/// §10), and the N150's plane scans the firmware's framebuffer. When a
/// buffer can reach a plane -- a card node, hyprix's direct scan-out of a
/// client's buffer or a Present flip -- this turns `true` (or becomes per object)
/// and every object that may reach a plane is mapped
/// write-combining as asked: the display engine does not snoop the CPU's
/// caches, so a write-back mapping's writes could stay in them.
pub const SCANOUT: bool = false;

/// Whether a write-combining mmap of `entry` is served write-back.
///
/// anv maps scan-out and external objects write-combining
/// (`anv_bo_get_mmap_mode`), and Mesa's software WSI allocates its linear
/// staging buffer as scan-out (`wsi_create_buffer_blit_context`), so every
/// frame the WSI copies to the X server is read by the CPU through a
/// write-combining mapping: uncached reads, tens of milliseconds for a
/// 1878x1158 frame. Write-combining buys nothing where the CPU's caches and
/// the GPU agree, so it is dropped where all of these hold:
///
/// * the part has an LLC the GPU snoops ([`HAS_LLC`]);
/// * the object's PAT index is `PAT_WB`, `I915_CACHE_LLC`, so the GPU's
///   accesses to it go through the LLC, coherent with the CPU's;
/// * nothing can take it to a display plane ([`SCANOUT`]);
/// * it was never exported (`shared`): a dmabuf may reach another device or,
///   once DRI3 exists, a plane.
///
/// i915 itself maps such an object write-combining when asked; the
/// difference is only speed, since on an LLC part both mappings see the
/// GPU's writes. An object served so may not leave `PAT_WB` afterwards
/// ([`set_caching`]).
pub fn wc_served_cached<M>(entry: &Object<M>) -> bool {
    HAS_LLC && !SCANOUT && entry.pat_index == PAT_WB && !entry.shared
}

/// i915's PAT index for `I915_CACHE_NONE` on Gen12 (index 3, UC).
const PAT_UC: u8 = 3;
/// i915's PAT index for `I915_CACHE_WT` on Gen12 (index 2).
const PAT_WT: u8 = 2;

/// A new object of `size` bytes (rounded up to a page) and a handle for it
/// in `file`: `i915_gem_object_create_user` and `i915_gem_publish`.
///
/// # Errors
///
/// `EINVAL` for size 0, `E2BIG` past `INT_MAX` pages, `ENOMEM` for a full
/// table, and what the back end refused the memory with.
pub fn new_object<B: Backend>(
    frontend: &mut Frontend<B>,
    file: u8,
    size: u64,
) -> Result<(u32, u64), Errno> {
    let size = size.checked_add(PAGE - 1).ok_or(Errno::E2BIG)? & !(PAGE - 1);
    if size == 0 {
        return Err(Errno::EINVAL);
    }
    if size / PAGE > MAX_PAGES {
        return Err(Errno::E2BIG);
    }
    if frontend.state.objects.len() == limits::OBJECTS
        || frontend.state.handles.len() == limits::HANDLES
    {
        return Err(Errno::ENOMEM);
    }
    let memory = frontend.backend.create_memory(size)?;
    let state = &mut frontend.state;
    let object = state
        .objects
        .insert(Object {
            memory,
            size,
            handles: 0,
            dmabuf_alive: false,
            shared: false,
            tiling: gem::TILING_NONE,
            stride: 0,
            caching: gem::CACHING_CACHED,
            madv: gem::MADV_WILLNEED,
            pat_index: PAT_WB,
            mmap_files: 0,
            mapped_wc: None,
            wc_as_cached: false,
            write: FenceRef::Signalled(0),
            reads: [FenceRef::Signalled(0); limits::OBJECT_READS],
            bindings: 0,
        })
        .ok_or(Errno::ENOMEM)?;
    let object = u16::try_from(object).map_err(|_| Errno::ENOMEM)?;
    match new_handle(state, file, object) {
        Ok(handle) => Ok((handle, size)),
        Err(errno) => {
            free_object(state, object);
            Err(errno)
        }
    }
}

/// A new handle of `file` for `object`.
fn new_handle<M>(state: &mut Tables<M>, file: u8, object: u16) -> Result<u32, Errno> {
    let slot = state
        .handles
        .insert(Handle { file, object })
        .ok_or(Errno::ENOMEM)?;
    if let Some(entry) = state.objects.get_mut(usize::from(object)) {
        entry.handles += 1;
    }
    u32::try_from(slot + 1).map_err(|_| Errno::ENOMEM)
}

/// Take object slot `object` out of its table, bumping its generation.
fn free_object<M>(state: &mut Tables<M>, object: u16) {
    let _ = state.objects.remove(usize::from(object));
    if let Some(generation) = state.generations.get_mut(usize::from(object)) {
        *generation = generation.wrapping_add(1);
    }
}

/// The object a file's handle names.
///
/// # Errors
///
/// `ENOENT`.
pub fn lookup<M>(state: &Tables<M>, file: u8, handle: u32) -> Result<u16, Errno> {
    state.object_of(file, handle).ok_or(Errno::ENOENT)
}

/// `i915_gem_create_ioctl`: system memory, the only region.
pub fn create<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = GemCreate::from_bytes(data).ok_or(Errno::EFAULT)?;
    let (handle, size) = new_object(frontend, call.file, args.size)?;
    args.handle = handle;
    args.size = size;
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_create_ext_ioctl`: the regions must be system memory alone
/// (`set_placements`), `SET_PAT` is for `Xe_LPG` on (`ext_set_pat`:
/// `ENODEV` on Gen12.0), and protected content needs PXP (`ENODEV`).
pub fn create_ext<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = GemCreateExt::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags & !gem::CREATE_EXT_FLAG_NEEDS_CPU_ACCESS != 0 {
        return Err(Errno::EINVAL);
    }
    let mut regions = None;
    let mut twice = false;
    super::vm::walk_extensions(host, call, args.extensions, &mut |name, at| match name {
        gem::CREATE_EXT_MEMORY_REGIONS => {
            // A second one finds `ext_data->n_placements` set: `EINVAL`.
            twice |= regions.replace(at).is_some();
            Ok(())
        }
        gem::CREATE_EXT_PROTECTED_CONTENT => Err(Errno::ENODEV),
        gem::CREATE_EXT_SET_PAT => Err(Errno::ENODEV),
        _ => Err(Errno::EINVAL),
    })?;
    if let Some(at) = regions {
        check_placements(host, call, at)?;
    }
    if twice {
        return Err(Errno::EINVAL);
    }
    // `NEEDS_CPU_ACCESS` asks for two placements, LMEM and SMEM; there is
    // only one.
    if args.flags & gem::CREATE_EXT_FLAG_NEEDS_CPU_ACCESS != 0 {
        return Err(Errno::EINVAL);
    }
    let (handle, size) = new_object(frontend, call.file, args.size)?;
    args.handle = handle;
    args.size = size;
    put(data, &args.to_bytes());
    Ok(())
}

/// `GEM_CLOSE` (`drm_gem_close_ioctl`).
pub fn close<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = GemClose::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.pad != 0 {
        return Err(Errno::EINVAL);
    }
    drop_handle(&mut frontend.state, call.file, args.handle)
}

/// Drop one handle; the object goes once nothing holds or uses it.
fn drop_handle<M>(state: &mut Tables<M>, file: u8, handle: u32) -> Result<(), Errno> {
    let object = lookup(state, file, handle)?;
    let slot = usize::try_from(handle).map_err(|_| Errno::EINVAL)? - 1;
    let _ = state.handles.remove(slot);
    if let Some(entry) = state.objects.get_mut(usize::from(object)) {
        entry.handles = entry.handles.saturating_sub(1);
        if entry.handles == 0 {
            entry.mmap_files = 0;
        }
    }
    Ok(())
}

/// A released file's handles.
pub fn release_file<M>(state: &mut Tables<M>, file: u8) {
    let mut doomed = [0_u32; 256];
    loop {
        let mut count = 0;
        for (slot, handle) in state.handles.iter() {
            if handle.file == file
                && let Some(cell) = doomed.get_mut(count)
            {
                *cell = u32::try_from(slot + 1).unwrap_or(0);
                count += 1;
            }
        }
        for handle in doomed.iter().take(count) {
            let _ = drop_handle(state, file, *handle);
        }
        if count < doomed.len() {
            break;
        }
    }
    let bit = 1_u32 << (file % 32);
    for (_, object) in state.objects.iter_mut() {
        object.mmap_files &= !bit;
    }
}

/// Free every object nothing holds, once idle: its bindings are unbound and
/// its memory dropped (`i915_gem_free_object`).
pub fn free_idle<B: Backend>(backend: &mut B, state: &mut Tables<B::Memory>) {
    let mut doomed = [u16::MAX; 64];
    loop {
        let mut count = 0;
        for (slot, object) in state.objects.iter() {
            if object.orphaned()
                && object.idle(|fence| state.requests.status(fence))
                && let Some(cell) = doomed.get_mut(count)
            {
                *cell = u16::try_from(slot).unwrap_or(u16::MAX);
                count += 1;
            }
        }
        for object in doomed.iter().take(count).copied() {
            unbind_all(backend, state, object);
            free_object(state, object);
        }
        if count < doomed.len() {
            break;
        }
    }
}

/// Unbind every binding of `object`.
fn unbind_all<B: Backend>(backend: &mut B, state: &mut Tables<B::Memory>, object: u16) {
    if state
        .objects
        .get(usize::from(object))
        .is_none_or(|entry| entry.bindings == 0)
    {
        return;
    }
    let mut index = 0;
    while let Some(binding) = state.bindings.as_slice().get(index).copied() {
        if binding.object != object {
            index += 1;
            continue;
        }
        if let Some(vm) = state
            .vms
            .get(usize::from(binding.vm()))
            .and_then(|vm| vm.backend)
        {
            backend.unbind(vm, binding.address(), binding.size);
        }
        let _ = state.bindings.remove(index);
    }
    if let Some(entry) = state.objects.get_mut(usize::from(object)) {
        entry.bindings = 0;
    }
}

/// The fake offset an mmap of object slot `object` in `mode` is asked at:
/// the slot, its generation and the mode above 40 bits of byte offset, so
/// an offset of a freed object names nothing.
fn fake_offset<M>(state: &Tables<M>, object: u16, wc: bool) -> u64 {
    let generation = state
        .generations
        .get(usize::from(object))
        .copied()
        .unwrap_or(0);
    (u64::from(generation) << 56) | ((u64::from(object) + 1) << 42) | (u64::from(wc) << 41)
}

/// `i915_gem_mmap_offset_ioctl`: `WB` and `WC` (K6) are served. `GTT` maps
/// through the aperture and its fence registers, `UC` and `FIXED` need
/// what this driver does not do; each is `ENODEV`, which anv and iris read
/// as absent.
pub fn mmap_offset<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = GemMmapOffset::from_bytes(data).ok_or(Errno::EFAULT)?;
    super::vm::walk_extensions(host, call, args.extensions, &mut |_, _| Err(Errno::EINVAL))?;
    let wc = match args.flags {
        gem::MMAP_OFFSET_WB => false,
        gem::MMAP_OFFSET_WC => true,
        gem::MMAP_OFFSET_GTT | gem::MMAP_OFFSET_UC | gem::MMAP_OFFSET_FIXED => {
            return Err(Errno::ENODEV);
        }
        _ => return Err(Errno::EINVAL),
    };
    let state = &mut frontend.state;
    let object = lookup(state, call.file, args.handle)?;
    if let Some(entry) = state.objects.get_mut(usize::from(object)) {
        entry.mmap_files |= 1 << (call.file % 32);
    }
    args.offset = fake_offset(state, object, wc);
    put(data, &args.to_bytes());
    Ok(())
}

/// An mmap of the node at `offset`, `pages` long: the object's VMO from the
/// offset's byte on, if the file asked for that offset (`drm_gem_mmap`,
/// `i915_gem_mmap`).
pub fn mmap<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    request: u64,
    file: u64,
    offset: u64,
    pages: u32,
) {
    let answer = (|| -> Result<(), Errno> {
        let state = &mut frontend.state;
        let file = state.file_slot(file).ok_or(Errno::EBADF)?;
        let object = u16::try_from((offset >> 42) & 0x3fff)
            .ok()
            .and_then(|slot| slot.checked_sub(1))
            .ok_or(Errno::EINVAL)?;
        let generation = (offset >> 56) as u8;
        let wc = offset & (1 << 41) != 0;
        let within = offset & ((1 << 41) - 1);
        if state.generations.get(usize::from(object)) != Some(&generation) {
            return Err(Errno::EINVAL);
        }
        let entry = state
            .objects
            .get_mut(usize::from(object))
            .ok_or(Errno::EINVAL)?;
        if entry.mmap_files & (1 << (file % 32)) == 0 {
            return Err(Errno::EACCES);
        }
        let end = within
            .checked_add(u64::from(pages) * PAGE)
            .ok_or(Errno::EINVAL)?;
        if !within.is_multiple_of(PAGE) || end > entry.size || pages == 0 {
            return Err(Errno::EINVAL);
        }
        let asked_wc = wc;
        let wc = wc && !wc_served_cached(entry);
        // K6: a VMO turns write-combining only before anything maps it, and
        // then stays so. An object asked in both modes keeps its first.
        match entry.mapped_wc {
            Some(first) if first != wc => return Err(Errno::EINVAL),
            _ => {}
        }
        // Backed before the client's faults can commit its pages one by one
        // (`Backend::populate`).
        frontend.backend.populate(&entry.memory)?;
        host.reply_map(request, &entry.memory, within, wc)?;
        entry.mapped_wc = Some(wc);
        entry.wc_as_cached |= asked_wc && !wc;
        Ok(())
    })();
    if let Err(errno) = answer {
        host.reply(request, -i32::from(errno.0));
    }
}

/// `i915_gem_set_tiling_ioctl`: X and Y with a stride are recorded. Gen12
/// has no fence registers for them and no bit-6 swizzling.
pub fn set_tiling<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = SetTiling::from_bytes(data).ok_or(Errno::EFAULT)?;
    let object = lookup(&frontend.state, call.file, args.handle)?;
    // `i915_tiling_ok`.
    if args.tiling_mode > gem::TILING_LAST {
        return Err(Errno::EINVAL);
    }
    if args.tiling_mode == gem::TILING_NONE {
        args.stride = 0;
    } else {
        let tile_width = if args.tiling_mode == gem::TILING_Y {
            128
        } else {
            512
        };
        if args.stride == 0 || args.stride > 256 * 1024 || args.stride % tile_width != 0 {
            return Err(Errno::EINVAL);
        }
    }
    let entry = frontend
        .state
        .objects
        .get_mut(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    entry.tiling = args.tiling_mode;
    entry.stride = args.stride;
    args.swizzle_mode = gem::BIT_6_SWIZZLE_NONE;
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_get_tiling_ioctl`.
pub fn get_tiling<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = GetTiling::from_bytes(data).ok_or(Errno::EFAULT)?;
    let object = lookup(&frontend.state, call.file, args.handle)?;
    let entry = frontend
        .state
        .objects
        .get(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    args.tiling_mode = entry.tiling;
    args.swizzle_mode = gem::BIT_6_SWIZZLE_NONE;
    args.phys_swizzle_mode = gem::BIT_6_SWIZZLE_NONE;
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_set_caching_ioctl`: the level picks the PAT index the
/// object's bindings use (`i915_gem_get_pat_index`, Gen12's table).
pub fn set_caching<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = Caching::from_bytes(data).ok_or(Errno::EFAULT)?;
    let pat = match args.caching {
        gem::CACHING_NONE => PAT_UC,
        gem::CACHING_CACHED => PAT_WB,
        gem::CACHING_DISPLAY => PAT_WT,
        _ => return Err(Errno::EINVAL),
    };
    let object = lookup(&frontend.state, call.file, args.handle)?;
    let entry = frontend
        .state
        .objects
        .get_mut(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    // A write-combining mapping served write-back ([`wc_served_cached`]) is
    // coherent only while the GPU goes through the LLC.
    if entry.wc_as_cached && pat != PAT_WB {
        return Err(Errno::EBUSY);
    }
    entry.caching = args.caching;
    entry.pat_index = pat;
    Ok(())
}

/// `i915_gem_get_caching_ioctl`.
pub fn get_caching<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = Caching::from_bytes(data).ok_or(Errno::EFAULT)?;
    let object = lookup(&frontend.state, call.file, args.handle)?;
    args.caching = frontend
        .state
        .objects
        .get(usize::from(object))
        .ok_or(Errno::ENOENT)?
        .caching;
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_set_domain_ioctl`: the domains are checked as i915 checks them
/// and change nothing; the CPU and the GPU are coherent through the LLC.
pub fn set_domain<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SetDomain::from_bytes(data).ok_or(Errno::EFAULT)?;
    let known = gem::DOMAIN_CPU | gem::DOMAIN_GTT | gem::DOMAIN_WC;
    if args.write_domain & !known != 0 || args.read_domains & !known != 0 {
        return Err(Errno::EINVAL);
    }
    if args.write_domain != 0 && args.read_domains != args.write_domain {
        return Err(Errno::EINVAL);
    }
    lookup(&frontend.state, call.file, args.handle).map(|_| ())
}

/// `i915_gem_sw_finish_ioctl`: a handle that names an object.
pub fn sw_finish<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let handle = u32::from_le_bytes(
        data.get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or(Errno::EFAULT)?,
    );
    lookup(&frontend.state, call.file, handle).map(|_| ())
}

/// `i915_gem_madvise_ioctl`: recorded; nothing is purged, so `retained` is
/// always 1.
pub fn madvise<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = Madvise::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.madv != gem::MADV_WILLNEED && args.madv != gem::MADV_DONTNEED {
        return Err(Errno::EINVAL);
    }
    let object = lookup(&frontend.state, call.file, args.handle)?;
    if let Some(entry) = frontend.state.objects.get_mut(usize::from(object)) {
        entry.madv = args.madv;
    }
    args.retained = 1;
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_busy_ioctl`: the engine classes still reading (one bit each
/// from bit 16) and the one still writing (its class plus one, also as a
/// reader), as `__busy_read_flag` and `__busy_write_id` encode them.
pub fn busy<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = GemBusy::from_bytes(data).ok_or(Errno::EFAULT)?;
    let state = &frontend.state;
    let object = lookup(state, call.file, args.handle)?;
    let entry = state
        .objects
        .get(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    let class_of = |fence: FenceRef| -> Option<u16> {
        if state.requests.status(fence).is_some() {
            return None;
        }
        Some(match fence {
            FenceRef::Request(id) => state
                .requests
                .get(id)
                .map_or(query::ENGINE_CLASS_RENDER, |request| request.engine.class),
            // A merged fence: some engine, render being the one in use.
            _ => query::ENGINE_CLASS_RENDER,
        })
    };
    let mut busy = 0_u32;
    if let Some(class) = class_of(entry.write) {
        busy |= (u32::from(class) + 1) | (0x10000 << class);
    }
    for read in entry.reads {
        if let Some(class) = class_of(read) {
            busy |= 0x10000 << class;
        }
    }
    args.busy = busy;
    put(data, &args.to_bytes());
    Ok(())
}

/// `i915_gem_wait_ioctl`: wait for every access to the object, up to
/// `timeout_ns` (negative: for ever; 0: poll), held as a deferred reply.
pub fn wait<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<Option<Hold>, Errno> {
    let args = GemWait::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags != 0 {
        return Err(Errno::EINVAL);
    }
    let state = &frontend.state;
    let object = lookup(state, call.file, args.bo_handle)?;
    let entry = state
        .objects
        .get(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    if entry.idle(|fence| state.requests.status(fence)) {
        // Done at once: no time passed.
        return Ok(None);
    }
    if args.timeout_ns == 0 {
        return Err(ETIME);
    }
    let now = host.now_ns();
    let deadline = (args.timeout_ns > 0).then(|| now.saturating_add(args.timeout_ns as u64));
    let generation = state
        .generations
        .get(usize::from(object))
        .copied()
        .unwrap_or(0);
    Ok(Some(Hold {
        deadline,
        kind: WaitKind::Object {
            object,
            generation,
            start_ns: now,
        },
    }))
}

/// A held `GEM_WAIT`: `Some(status)` once settled, with the time left
/// written back as i915 writes it.
pub fn evaluate_wait<M>(
    state: &Tables<M>,
    object: u16,
    generation: u8,
    start_ns: u64,
    data: &mut [u8; 48],
    now: u64,
    expired: bool,
) -> Option<i32> {
    let mut args = GemWait::from_bytes(data)?;
    let gone = state.generations.get(usize::from(object)) != Some(&generation);
    let idle = gone
        || state
            .objects
            .get(usize::from(object))
            .is_none_or(|entry| entry.idle(|fence| state.requests.status(fence)));
    if !idle && !expired {
        return None;
    }
    let status = if idle { 0 } else { -i32::from(ETIME.0) };
    if args.timeout_ns > 0 {
        let spent = i64::try_from(now.saturating_sub(start_ns)).unwrap_or(i64::MAX);
        args.timeout_ns = (args.timeout_ns - spent).max(0);
        if !idle {
            args.timeout_ns = 0;
        }
    }
    put(data, &args.to_bytes());
    Some(status)
}

/// `i915_gem_get_aperture_ioctl`: the GGTT's size, all of it available to
/// nobody else's objects (none are pinned there for programs).
pub fn get_aperture<B: Backend>(frontend: &mut Frontend<B>, data: &mut [u8]) -> Result<(), Errno> {
    let facts = *frontend.backend.facts();
    let args = GetAperture {
        aper_size: facts.ggtt_bytes,
        aper_available_size: facts.ggtt_bytes,
    };
    put(data, &args.to_bytes());
    Ok(())
}

/// `PRIME_HANDLE_TO_FD` (`drm_prime_handle_to_fd_ioctl`): the object's
/// dmabuf, one per object (`obj->dma_buf`), as a new descriptor.
pub fn prime_handle_to_fd<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = PrimeHandle::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags & !(consts::CLOEXEC | consts::RDWR) != 0 {
        return Err(Errno::EINVAL);
    }
    let state = &mut frontend.state;
    let object = lookup(state, call.file, args.handle)?;
    let cookie = state.dmabuf_cookie(object);
    let entry = state
        .objects
        .get_mut(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    // Backed before anyone else can touch it (`Backend::populate`).
    frontend.backend.populate(&entry.memory)?;
    let installed = host.dmabuf_install(
        call.request,
        &entry.memory,
        cookie,
        args.flags & consts::RDWR != 0,
        args.flags & consts::CLOEXEC != 0,
    )?;
    if installed.made {
        entry.dmabuf_alive = true;
    }
    entry.shared = true;
    args.fd = installed.fd;
    put(data, &args.to_bytes());
    Ok(())
}

/// `PRIME_FD_TO_HANDLE` (`drm_gem_prime_fd_to_handle`): a dmabuf this
/// driver made names its object, and the file's existing handle for it is
/// given back if it has one. A dmabuf of another driver cannot be imported:
/// its memory is not one this driver can bind.
pub fn prime_fd_to_handle<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = PrimeHandle::from_bytes(data).ok_or(Errno::EFAULT)?;
    let Ok(cookie) = host.dmabuf_resolve(call.request, args.fd) else {
        return Err(unserved(
            frontend,
            host,
            call,
            "PRIME_FD_TO_HANDLE of another driver's dmabuf",
        ));
    };
    let state = &mut frontend.state;
    let object = state.object_of_cookie(cookie).ok_or(Errno::ENOENT)?;
    let existing = state
        .handles
        .iter()
        .find(|(_, handle)| handle.file == call.file && handle.object == object)
        .map(|(slot, _)| slot);
    args.handle = match existing {
        Some(slot) => u32::try_from(slot + 1).map_err(|_| Errno::ENOMEM)?,
        None => new_handle(state, call.file, object)?,
    };
    put(data, &args.to_bytes());
    Ok(())
}

/// A dmabuf of this driver's went: its object is no longer held by it.
pub fn dmabuf_released<M>(state: &mut Tables<M>, cookie: u64) {
    if let Some(object) = state.object_of_cookie(cookie)
        && let Some(entry) = state.objects.get_mut(usize::from(object))
    {
        entry.dmabuf_alive = false;
    }
}

/// A program's `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` on one of this driver's
/// dmabufs (K5): its fence joins the object's implicit sync, as a write if
/// asked for writing (`DMA_BUF_SYNC_WRITE`), else as a read. A fence of
/// another driver cannot be waited on here, and is refused (`EOPNOTSUPP`).
pub fn dmabuf_import<M>(
    state: &mut Tables<M>,
    cookie: u64,
    flags: u32,
    kind: u32,
    value: u64,
) -> Result<(), Errno> {
    use ferrix_chardevctl_consts::{IMPORT_FOREIGN, IMPORT_OWN, IMPORT_SIGNALLED};
    const READ: u32 = 1;
    const WRITE: u32 = 2;
    if flags & !(READ | WRITE) != 0 || flags == 0 {
        return Err(Errno::EINVAL);
    }
    let object = state.object_of_cookie(cookie).ok_or(Errno::ENOENT)?;
    let fence = match kind {
        IMPORT_SIGNALLED => FenceRef::Signalled(0),
        IMPORT_OWN => state
            .kernel_fences
            .iter()
            .find(|(_, kernel)| kernel.cookie == value)
            .map_or(FenceRef::Signalled(0), |(_, kernel)| kernel.fence),
        IMPORT_FOREIGN => return Err(Errno::EOPNOTSUPP),
        _ => return Err(Errno::EINVAL),
    };
    let entry = state
        .objects
        .get_mut(usize::from(object))
        .ok_or(Errno::ENOENT)?;
    if flags & WRITE != 0 {
        entry.write = entry.write.merge(fence);
    } else {
        add_read(entry, fence);
    }
    Ok(())
}

/// The chardev protocol's import kinds (`ferrix_chardevctl::message`),
/// spelt here so this crate needs no dependency on it; the driver's tests
/// hold the two to the same numbers.
pub mod ferrix_chardevctl_consts {
    /// `IMPORT_OWN`.
    pub const IMPORT_OWN: u32 = 1;
    /// `IMPORT_SIGNALLED`.
    pub const IMPORT_SIGNALLED: u32 = 2;
    /// `IMPORT_FOREIGN`.
    pub const IMPORT_FOREIGN: u32 = 3;
}

/// Add a read fence to `object`, merging the oldest when all are taken.
pub fn add_read<M>(object: &mut Object<M>, fence: FenceRef) {
    if let Some(slot) = object
        .reads
        .iter_mut()
        .find(|read| matches!(read, FenceRef::Signalled(0)))
    {
        *slot = fence;
        return;
    }
    let last = limits::OBJECT_READS - 1;
    if let (Some(first), Some(second)) =
        (object.reads.first().copied(), object.reads.get(1).copied())
    {
        object.reads.copy_within(1.., 0);
        if let Some(merged) = object.reads.first_mut() {
            *merged = first.merge(second);
        }
        if let Some(slot) = object.reads.get_mut(last) {
            *slot = fence;
        }
    }
}

/// Check a `MEMORY_REGIONS` extension (`set_placements`): its `pad` zero,
/// one or more regions, each system memory 0, none twice.
///
/// # Errors
///
/// `EINVAL`, `EFAULT`.
pub fn check_placements<M, H: Host<M>>(host: &mut H, call: &Call, at: u64) -> Result<(), Errno> {
    let mut raw = [0_u8; CreateExtMemoryRegions::SIZE];
    host.copy_in(call.request, at, &mut raw)
        .map_err(|_| Errno::EFAULT)?;
    let ext = CreateExtMemoryRegions::from_bytes(&raw).ok_or(Errno::EFAULT)?;
    // i915 has a region array of `INTEL_REGION_UNKNOWN` entries; any more
    // is "too large".
    const REGIONS_MAX: u32 = 8;
    if ext.pad != 0 || ext.num_regions == 0 || ext.num_regions > REGIONS_MAX {
        return Err(Errno::EINVAL);
    }
    let mut seen_system = false;
    for index in 0..ext.num_regions {
        let mut raw = [0_u8; MemoryClassInstance::SIZE];
        host.copy_in(
            call.request,
            ext.regions + u64::from(index) * MemoryClassInstance::SIZE as u64,
            &mut raw,
        )
        .map_err(|_| Errno::EFAULT)?;
        let region = MemoryClassInstance::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if region.memory_class != query::MEMORY_CLASS_SYSTEM
            || region.memory_instance != 0
            || seen_system
        {
            return Err(Errno::EINVAL);
        }
        seen_system = true;
    }
    Ok(())
}
