# N3b research: Chrome GPU compositing on hyprix with NVIDIA dmabufs

Date: 2026-10-03. Worktree `nvidia-n2`. This was read-only research: nothing was built and no VM was started. The only things compiled were two tiny host programs in the scratchpad, which print ioctl numbers from NVIDIA's headers (`scratchpad/n3b/n.c`, `e.c`).

Abbreviations used below:

* `N` = `~/.local/share/ferrix/nvidia/580.173.02/src/kernel-open`
* `K` = `~/.local/share/ferrix/nvidia/580.173.02/src/src/nvidia-modeset/kapi/src/nvkms-kapi.c`
* `L` = `~/.local/share/ferrix/nvidia/580.173.02/tree/usr/lib/x86_64-linux-gnu`
* `C` = `~/.local/share/ferrix/chrome/tree/chrome-window/chrome`

Claims that come from Chromium or Mesa source knowledge, and not from these files, are marked **[src-knowledge]**.

---

## 1. What Chrome needs to turn on GPU compositing (Ozone/Wayland)

Chrome here is Chrome for Testing 154.0.8037.57.

### 1.1 The Wayland side

* **`zwp_linux_dmabuf_v1`, v3 or v4.**
  * The binary has these strings: `zwp_linux_dmabuf_v1`, `zwp_linux_dmabuf_feedback_v1`, `Failed to bind zwp_linux_dmabuf_v1`, `Failed to map zwp_linux_dmabuf_feedback_v1 format table`, `main_device`.
  * **[src-knowledge]** Chrome binds up to v4 and uses feedback (format table, `main_device`, tranches) when the compositor offers it. Otherwise it uses v3 `format`/`modifier` events.
  * `wl_drm` is an older fallback for finding the device.
* **No dmabuf means no GPU compositing.**
  * The Wayland GPU files in the binary are `gbm_pixmap_wayland.cc`, `gbm_surfaceless_wayland.cc`, `gl_surface_wayland.cc`, `wayland_canvas_surface.cc` and `wayland_buffer_manager_gpu.cc`. There is no EGL-readback Wayland surface.
  * **[src-knowledge]** Without dmabuf plus GBM, native pixmaps are unsupported, and viz falls back to the software compositor over `WaylandCanvasSurface` and `wl_shm`. That is the path Ferrix uses today, forced by `--disable-gpu-compositing`.
* **Formats.** `DRM_FORMAT_ARGB8888`, `XRGB8888`, `ABGR8888`, `XBGR8888` and others appear.
  * **[src-knowledge]** `DRM_FORMAT_MOD_LINEAR` is accepted when the compositor advertises it, so **linear-only is fine**.
  * NVIDIA's GBM backend accepts an explicit modifier list containing LINEAR. It only rejects the flag `GBM_BO_USE_LINEAR` combined with a list (allocator string `GBM_BO_USE_LINEAR cannot be used with an explicit format modifier list`).
* **Explicit sync.** `wp_linux_drm_syncobj_manager_v1`, `wp_linux_drm_syncobj_surface_v1`, `WaylandSyncobjReleaseTimeline` and the switch `disable-explicit-dma-fences` are present. There is no feature gate, so Chrome uses explicit sync whenever the global exists. `zwp_linux_explicit_synchronization_v1` is absent.
  * **[src-knowledge]** Without syncobj, and without `EGL_ARM_implicit_external_sync` (which NVIDIA lacks), `GbmSurfacelessWayland` waits for the frame's GL fence on a worker thread before it commits the buffer. So with no kernel sync at all, a buffer hyprix receives is already complete. **Verify this on the first run** (look for tearing or partial frames).

### 1.2 The render node

* **Strings found:**
  * `ui/ozone/platform/wayland/common/drm_render_node_path_finder.cc` and `drm_render_node_handle.cc`;
  * `/dev/dri/renderD%d`, `Failed to find drm render node path.`;
  * statically linked libdrm: `drmGetDevices2() has not found any devices`, `drmGetDeviceFromDevId() returned an error`;
  * switch `render-node-override`.
* **[src-knowledge]** The path finder takes the first `renderD128…` whose driver name is not `vgem`. Recent Chromium can also take `main_device` from dmabuf feedback through `drmGetDeviceFromDevId`, which needs `/sys/dev/char/226:N`.
* **Consequence for Ferrix.** virtio-gpu is `renderD128`. Chrome would pick it, and Mesa's libgbm would then look for `virtio_gpu_gbm.so`, fall back to `dri_gbm.so` (which needs `libgallium-25.0.7…`, not in the tree), and fail.
  * **Chrome must be pointed at NVIDIA's node:** `--render-node-override=/dev/dri/renderD129`. Feedback `main_device` = 226:129 is the clean way once sysfs serves it (§4).

### 1.3 libgbm

* **Chrome cannot start without it.** `readelf -d chrome` shows `NEEDED libgbm.so.1`.
* **Calls used:** `gbm_create_device`, `gbm_bo_create`, `gbm_bo_create_with_modifiers` (not `…2`), `gbm_bo_import`, `gbm_bo_get_fd_for_plane`, `gbm_bo_get_modifier`, `gbm_bo_map`. No minigbm.
* **Mesa's libgbm is already in Chrome's tree:** Debian libgbm1 25.0.7 at `~/.local/share/ferrix/chrome/tree/usr/lib/x86_64-linux-gnu/libgbm.so.1`, from `chrome/pool/libgbm1_25.0.7-2+deb13u1_amd64.deb`.
  * Its strings: `GBM_BACKEND`, `GBM_BACKENDS_PATH`, `/usr/lib/x86_64-linux-gnu/gbm`, `%.*s/%s%s.so`, `_gbm`, `drmGetVersion`.
  * **[src-knowledge]** Backend order: `$GBM_BACKEND`, then `drmGetVersion(fd)->name` + `_gbm.so` in `$GBM_BACKENDS_PATH`, then `dri_gbm.so`.
  * So the node must answer `DRM_IOCTL_VERSION` with name **`nvidia-drm`**, and `GBM_BACKENDS_PATH` must reach `L/gbm/nvidia-drm_gbm.so`, a symlink to `../libnvidia-allocator.so.1`.
  * `fetch-nvidia.sh` `EXTRA_DEBS` has no libgbm1, and it does not need it, because Chrome's tree has it.
  * `libnvidia-egl-gbm.so.1` NEEDs `libgbm.so.1` too, but that is the EGL GBM *platform*, which Chrome on Wayland does not use.

### 1.4 How Chrome allocates and imports

* **GBM allocates.** Classes present: `GbmPixmapWayland::InitializeBuffer`, `GbmSurfacelessWayland::Present`, `CreateNativePixmapDmaBuf`.
* **ANGLE-Vulkan imports.** Present: `DmaBufImageSiblingVkLinux.cpp`, `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, `VK_KHR_external_memory_fd`, `VK_EXT_queue_family_foreign`, `EGL_EXT_image_dma_buf_import(_modifiers)`. ANGLE is linked into `chrome`.
* **So the GPU process uses two NVIDIA paths:**
  * NVIDIA's **GBM backend** (`libnvidia-allocator`), to allocate and export;
  * NVIDIA's **Vulkan driver** (`libGLX_nvidia` → `libnvidia-glcore`), to import the dmabuf as `VkDeviceMemory`.

### 1.5 Flags

* **Today** (`tools/common/xtask/src/chrome.rs:428-430`): `NVIDIA_GPU_FLAGS = "--use-angle=vulkan --enable-features=DefaultANGLEVulkan --ignore-gpu-blocklist --enable-gpu-rasterization --disable-gpu-compositing"`, used in `window_command_on` (`chrome.rs:449`) with `--ozone-platform=wayland`.
* **For N3b:**
  * drop `--disable-gpu-compositing`;
  * add `--render-node-override=/dev/dri/renderD129`;
  * set `GBM_BACKENDS_PATH=<volume>/usr/lib/x86_64-linux-gnu/gbm`, and optionally `GBM_BACKEND=nvidia-drm`.
  * Do **not** add `--use-vulkan` or `--enable-features=Vulkan`. Chrome refuses Skia-Vulkan on Wayland (string `'--ozone-platform=wayland' is not compatible with Vulkan`). The headless test's flags (`nvidia.rs:383`) are not right for the window case.

---

## 2. The DRM ioctls NVIDIA's userspace issues

### 2.1 Numbers

These were computed from `N/nvidia-drm/nvidia-drm-ioctl.h` and `<drm/drm.h>` (`scratchpad/n3b/ioctls.txt`). The table in the driver is at `N/nvidia-drm/nvidia-drm-drv.c:1752-1840`.

| nr | ioctl | number | struct (ioctl.h line) | render node? |
|---|---|---|---|---|
| 0x00 | GET_CRTC_CRC32 | 0xC0086440 | `:245` | yes, unused |
| 0x01 | GEM_IMPORT_NVKMS_MEMORY | 0xC0206441 | `:166` {mem_size, nvkms_params_ptr, nvkms_params_size, handle OUT} | yes |
| 0x02 | GEM_IMPORT_USERSPACE_MEMORY | 0xC0186442 | `:177` {size, address, handle OUT} | yes |
| 0x03 | GET_DEV_INFO | 0xC0246443 | `:183` {gpu_id, mig_device, primary_index, supports_alloc, generic_page_kind, page_kind_generation, sector_layout, supports_sync_fd, supports_semsurf} | yes |
| 0x04 | FENCE_SUPPORTED | 0x00006444 | – | yes, **unused** by 580 userspace |
| 0x05 | PRIME_FENCE_CONTEXT_CREATE | 0xC0306445 | `:199` | yes, **unused** |
| 0x06 | GEM_PRIME_FENCE_ATTACH | 0x40106446 | `:214` | yes, **unused** |
| 0x08 | GET_CLIENT_CAPABILITY | 0xC0106448 | `:221` | **no** (flags 0, `drv.c:1803-1805`) |
| 0x09 | GEM_EXPORT_NVKMS_MEMORY | 0xC0186449 | `:250` {handle, nvkms_params_ptr, nvkms_params_size} | yes |
| 0x0a | GEM_MAP_OFFSET | 0xC010644A | `:258` {handle, offset OUT} | yes |
| 0x0b | GEM_ALLOC_NVKMS_MEMORY | 0xC018644B | `:267` {handle OUT, block_linear, compressible IN/OUT, memory_size, flags (NO_SCANOUT=1)} | yes |
| 0x0c | GET_CRTC_CRC32_V2 | 0xC01C644C | `:240` | yes, unused |
| 0x0d | GEM_EXPORT_DMABUF_MEMORY | 0xC018644D | `:278` | yes |
| 0x0e | GEM_IDENTIFY_OBJECT | 0xC008644E | `:294` {handle, object_type OUT: NVKMS 0, DMABUF 1, USERMEMORY 2} | yes |
| 0x0f | DMABUF_SUPPORTED | 0x0000644F | – | yes |
| 0x10/0x11 | GET_DPY_ID_FOR_CONNECTOR_ID / reverse | 0xC0086450/1 | `:299,304` | yes, display only |
| 0x12/0x13 | GRANT/REVOKE_PERMISSIONS | 0xC00C6452/0xC0086453 | `:314,320` | master only |
| 0x14 | SEMSURF_FENCE_CTX_CREATE | 0xC0206454 | `:325` | yes |
| 0x15 | SEMSURF_FENCE_CREATE | 0xC0186455 | `:337` (fd OUT = sync_file) | yes |
| 0x16 | SEMSURF_FENCE_WAIT | 0x40186456 | `:361` (fd IN) | yes |
| 0x17 | SEMSURF_FENCE_ATTACH | 0x40186457 | `:378` | yes |
| 0x18 | GET_DRM_FILE_UNIQUE_ID | 0xC0086458 | `:395` {id OUT} | yes |

Core DRM ioctls:

* VERSION 0xC0406400, GET_CAP 0xC010640C, GEM_CLOSE 0x40086409;
* PRIME_HANDLE_TO_FD 0xC00C642D, PRIME_FD_TO_HANDLE 0xC00C642E;
* SYNCOBJ_CREATE 0xC00864BF, DESTROY 0xC00864C0, HANDLE_TO_FD 0xC01864C1, FD_TO_HANDLE 0xC01864C2, WAIT 0xC02864C3, RESET 0xC01064C4, SIGNAL 0xC01064C5, TIMELINE_WAIT 0xC03064CA, QUERY 0xC01864CB, TRANSFER 0xC02064CC, TIMELINE_SIGNAL 0xC01864CD.

On `/dev/nvidiactl`: `NV_ESC_EXPORT_TO_DMABUF_FD` = `_IOWR('F', 217, nv_ioctl_export_to_dma_buf_fd_t)` = **0xCA3046D9** (struct is 2608 bytes, `N/common/inc/nv-ioctl.h:134-148`, number at `nv-ioctl-numbers.h:41`).

`DRM_IOCTL_VERSION` answers (`drv.c:1920-1926`): name `nvidia-drm`, desc `NVIDIA DRM driver`, date `20160202`, version 0.0.0. Driver features are `DRIVER_GEM | DRIVER_RENDER | DRIVER_PRIME | DRIVER_SYNCOBJ | DRIVER_SYNCOBJ_TIMELINE` (`drv.c:1844-1851`).

### 2.2 Which userspace library uses which

**Method.** I scanned each library for 4-byte little-endian immediates of every number above (`scratchpad/n3b/scan.py`), plus objdump for the small `DRM_IO` ones. I also read `nm -D` and `strings` for the libdrm functions each one `dlopen`s. Every NVIDIA library `dlopen`s `libdrm.so.2`, so core DRM ioctls also come in through libdrm.

* **`gbm/nvidia-drm_gbm.so` = `libnvidia-allocator.so.580.173.02`** (the same file; md5 edaaf83e…; exports `gbmint_get_backend`):
  * **Immediates:**
    * GET_DEV_INFO ×2, GEM_ALLOC_NVKMS_MEMORY, GEM_MAP_OFFSET, GEM_CLOSE ×3, GEM_IMPORT_NVKMS_MEMORY, GEM_IMPORT_USERSPACE_MEMORY, GET_DRM_FILE_UNIQUE_ID;
    * `NV_ESC_EXPORT_TO_DMABUF_FD` (0xCA3046D9, at 0x7218).
  * **libdrm, by name:** `drmGetVersion`, `drmIoctl`, `drmCommandWriteRead`, `drmPrimeHandleToFD`, `drmPrimeFDToHandle`.
  * **Source files named in strings:**
    * `src/gbm_drv_common.c`: `gbm_drv_bo_create`, `gbm_drv_bo_import`, `gbm_drv_bo_get_plane_fd`, `PrimeHandleToFD failed`, `PrimeFDToHandle failed`.
    * `src/nv_gbm.c`: `nv_gbm_bo_create`, `nv_gbm_bo_map`, `DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY failed`, `DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET failed`.
    * `src/nv_gbm_common.c`: `DRM_IOCTL_NVIDIA_GET_DEV_INFO failed`, and a `libnvtegrahv.so`/`NvHvCheckOsNative` probe.
    * `src/nvrm_gbm.c`: `libnvrm_mem.so`, `NvRmMemHandleAllocAttr`, "nvmap handle". This is the **Tegra** path and is not used on a dGPU.
    * `src/gem_global_reference_table.cpp`: GEM handle refcounting, since libgbm and EGL share one fd.
  * **Device scan strings:** `card%d`, `/dev/dri`, `renderD`, `tegra-udrm`, `virtio_gpu`, `nvidia-drm`, `No direct render devices found.`
  * Formats offered: `XR24 AR24 XB24 AB24 AB30 XB30 AR30 BA30 RA30 XB4H AB4H`.
  * There is also an `NvRmShim*` API (`NvRmShimAllocMem`, `ExportMemContextToFd`, `ImportMemContextFromFd`, `ExportMemContextToDmabufFd`): an RM client of its own on `/dev/nvidiactl`.
* **`libnvidia-eglcore` and `libnvidia-glcore`** (the latter holds the Vulkan driver behind `libGLX_nvidia`), and also `libnvidia-vksc-core`. All three show an identical set:
  * PRIME_HANDLE_TO_FD (immediate), GEM_CLOSE, GEM_IMPORT_NVKMS_MEMORY ×2, GEM_IMPORT_USERSPACE_MEMORY ×2, GET_DEV_INFO, GEM_EXPORT_NVKMS_MEMORY, GEM_EXPORT_DMABUF_MEMORY, GEM_IDENTIFY_OBJECT, **DMABUF_SUPPORTED** (eglcore at 0xa0e35d, glcore at 0xa24c49);
  * GET_DPY_ID/GET_CONNECTOR_ID, GRANT/REVOKE_PERMISSIONS, the four SEMSURF_FENCE_*;
  * `NV_ESC_EXPORT_TO_DMABUF_FD`;
  * libdrm: `drmPrimeFDToHandle`, `drmPrimeHandleToFD`, `drmGetVersion`, `drmSyncobjCreate`/`Destroy`/`HandleToFD`/`ExportSyncFile`/`ImportSyncFile`/`TimelineWait`/`Transfer`;
  * strings `create_prime_buffer`, `zwp_linux_dmabuf_v1`, `wp_linux_drm_syncobj_*` (NVIDIA's own Vulkan Wayland WSI), `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`.
* **`libnvidia-glsi`:** GET_CLIENT_CAPABILITY, GEM_IMPORT/EXPORT_NVKMS_MEMORY, GEM_EXPORT_DMABUF_MEMORY, GEM_IDENTIFY_OBJECT, GET_DEV_INFO, `NV_ESC_EXPORT_TO_DMABUF_FD`; libdrm `drmGetCap`, `drmGetBusid`, `drmPrimeFDToHandle`/`HandleToFD`, `drmGetVersion`.
* **`libnvidia-egl-wayland`:** `drmGetDevice`, `drmGetVersion`, `drmSyncobj*` (explicit sync for EGL Wayland clients). Chrome does not use it.
* **Used by none of the 580 userspace libraries:** FENCE_SUPPORTED, PRIME_FENCE_CONTEXT_CREATE, GEM_PRIME_FENCE_ATTACH, CRC32, CRC32_V2.

### 2.3 What each needed ioctl does in nvidia-drm, and what implements it

* **VERSION.** DRM core, from `nv_drm_driver` (`drv.c:1920`).
* **GET_CAP.** DRM core. On a render node, Linux answers `DRM_CAP_PRIME` (5, IMPORT|EXPORT=3), `DRM_CAP_SYNCOBJ` (0x13) and `DRM_CAP_SYNCOBJ_TIMELINE` (0x14). glsi calls `drmGetCap` (there are `mov $0x5,%esi` sites, consistent with CAP_PRIME). **Answer PRIME=3, and SYNCOBJ/TIMELINE=1 only once they exist.**
* **GET_DEV_INFO** (`drv.c:1029-1063`):
  * `gpu_id` and `mig_device`;
  * `primary_index` = card minor; it returns `-ENOENT` if there is no primary;
  * `supports_alloc` = 1 when NVKMS is up, with `generic_page_kind`, `page_kind_generation` and `sector_layout` from `nvKms->getDeviceResourcesInfo` caps (`drv.c:748-762`);
  * `supports_semsurf` and `supports_sync_fd` when `semsurf_stride != 0`.
  * In nvrm, all of this comes from the KAPI device that `os/glue/kms.c` already opens. Setting `supports_sync_fd=0` and `supports_semsurf=0` at first keeps userspace off the fence ioctls.
* **GEM_ALLOC_NVKMS_MEMORY** (`gem-nvkms-memory.c:497-565`):
  * `nvKms->allocateMemory` (`K:1618`) with layout pitch or block-linear, type SCANOUT or OFFSCREEN (NO_SCANOUT), and `useVideoMemory = hasVideoMemory`. With NO_SCANOUT, a failed vidmem allocation falls back to sysmem.
  * That calls `nvKmsKapiAllocateVideoMemory` or `nvKmsKapiAllocateSystemMemory` (`K:627`), which is `NV01_MEMORY_SYSTEM` with `LOCATION_PCI`, `GPU_CACHEABLE_NO`, `COHERENCY_WRITE_BACK` when IO-coherent (WRITE_COMBINE otherwise) and NONCONTIGUOUS (`K:717-731`).
  * Then `__nv_drm_nvkms_gem_obj_init` (`:278-323`) collects sysmem pages with `nvKms->getMemoryPages` (`K:2053`, RM `NV003E_CTRL_CMD_GET_SURFACE_NUM_PHYS_PAGES`/`GET_SURFACE_PHYS_PAGES`). Vidmem has no pages.
  * **In nvrm, buffer placement is our policy:** nvrm implements this ioctl, so it can force sysmem for pitch-linear buffers.
* **GEM_MAP_OFFSET** (`gem.c:217-248`): returns the fake mmap offset. mmap of the render node at that offset (`nv_drm_mmap`, `gem.c:250-308`) then maps:
  * sysmem: each page's pfn (`gem-nvkms-memory.c:85-140`);
  * vidmem: `nvKms->mapMemory(…USER)`, a BAR1 address mapped WC (`:147-191`).
  * Used by `nv_gbm_bo_map` (CPU access via GBM).
* **GEM_IMPORT_NVKMS_MEMORY** (`:397-446`): `nvKms->importMemory` (`K:1661-1787`) copies in `NvKmsKapiPrivImportMemoryParams{int memFd; surfaceParams}` (`kapi/interface/nvkms-kapi-private.h:52-55`). It then runs RM `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD` on **memFd, an `/dev/nvidiactl` fd of the caller** onto which userspace's own RM client exported its allocation with `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD`, and finally `NV0041_CTRL_CMD_GET_SURFACE_INFO` to learn vid or sys.
  * **This is how userspace turns its own RM allocation (an EGL/Vulkan image) into a GEM handle, for export.**
* **GEM_EXPORT_NVKMS_MEMORY** (`:448-495`): `nvKms->exportMemory` (`K:1846-1925`) runs RM `NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD` of the GEM's RM memory onto the caller's nvidiactl `memFd`. Userspace then imports that into its own RM client.
  * **This is how an imported dmabuf becomes RM memory in an EGL or Vulkan context.**
* **GEM_EXPORT_DMABUF_MEMORY** (`gem-dma-buf.c:166-247`): the same, for a GEM made from a *foreign* dma-buf. It uses `nvKms->getSystemMemoryHandleFromSgt`, which is RM `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` with `NVOS32_DESCRIPTOR_TYPE_OS_SGT_PTR` (`K:~1965-2010`) over the importer's sg_table.
* **GEM_IMPORT_USERSPACE_MEMORY** (`gem-user-memory.c:185-`): pins user pages into a GEM. Not needed for N3b.
* **GEM_IDENTIFY_OBJECT** (`gem.c:310-345`): NVKMS, DMABUF or USERMEMORY, so userspace knows whether to use EXPORT_NVKMS or EXPORT_DMABUF.
* **DMABUF_SUPPORTED** (`drv.c:1074-1082`): returns 0 when NVKMS is up.
* **GET_DRM_FILE_UNIQUE_ID** (`drv.c:1066-1072`): a per-open counter (`nv_drm_open`, `drv.c:1551-1557`).
* **PRIME_HANDLE_TO_FD:** `drm_gem_prime_handle_to_fd` → `drm_gem_prime_export` (`drv.c:1875-1904`). One `dma_buf` per GEM object, cached.
* **PRIME_FD_TO_HANDLE:** `nv_drm_gem_prime_import` (`gem.c:148-176`):
  * a dma-buf of **this** device gives back the **same GEM object** (core `drm_gem_prime_import`);
  * one from another NV device: `prime_dup` → `nvKms->dupMemory` (`K:1789-1844`, `nvRmApiDupObject`);
  * a foreign one: attach + `gem_prime_import_sg_table` (`gem-dma-buf.c:132-164`).
  * **No sg_table is involved between NVIDIA clients of the same GPU.**
* **GEM_CLOSE:** DRM core. It drops the handle; `nv_drm_gem_free` → `nvKms->freeMemory` → `nvRmApiFree`.
* **SYNCOBJ_\*:** DRM core over dma_fences. NVIDIA's userspace makes its fences as sync_files with SEMSURF_FENCE_CREATE (a semaphore surface in RM; `nvidia-drm-fence.c`), imports them into syncobjs (`drmSyncobjImportSyncFile` = FD_TO_HANDLE with the IMPORT_SYNC_FILE flag), and moves timeline points (`drmSyncobjTransfer`).

### 2.4 Which ioctls each job needs

**(a) Allocate a buffer and export it as a dmabuf fd** (Chrome GPU process: GBM):
* VERSION (`nvidia-drm`) and GET_DEV_INFO (`supports_alloc=1`, `primary_index`) at `gbm_create_device`;
* GEM_ALLOC_NVKMS_MEMORY (`block_linear=0` for LINEAR);
* PRIME_HANDLE_TO_FD (`gbm_bo_get_fd_for_plane`);
* GEM_CLOSE;
* GET_DRM_FILE_UNIQUE_ID (the reference table);
* `nv_gbm_bo_map` adds GEM_MAP_OFFSET plus mmap of the node.
* The other export route, from EGL/Vulkan (export of an RM-allocated image), is RM export to a memFd → GEM_IMPORT_NVKMS_MEMORY → PRIME_HANDLE_TO_FD.
* `NV_ESC_EXPORT_TO_DMABUF_FD` on nvidiactl is a third, RM-native dma-buf. Below I argue the GBM path for a dGPU does not need it, but its presence in the allocator means nvrm should answer it with a clean error, as it does today (`os/kept/nv.c:818-823`: `-EINVAL`).

**(b) Import a dmabuf fd as GEM, EGLImage or Vulkan memory** (Chrome's ANGLE-Vulkan importing its own GBM buffer; hyprix at N3c):
* PRIME_FD_TO_HANDLE (same device: the same object), GEM_IDENTIFY_OBJECT, GEM_EXPORT_NVKMS_MEMORY onto a fresh nvidiactl fd, then `NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD` on nvidiactl (an RM control nvrm already serves);
* GET_DEV_INFO and DMABUF_SUPPORTED to pick and validate the node;
* GEM_CLOSE.
* A foreign dmabuf (for example a virtio-gpu one) would need GEM_EXPORT_DMABUF_MEMORY plus RM's OS-descriptor path. **Not needed for N3b.**
* This is inferred from the import/export pairing in `nvkms-kapi.c` and the ioctl set in `glcore`/`eglcore`. The exact call order inside the closed driver is unverified; an strace-like trace of the first run will show it.

**(c) mmap a buffer for the CPU** (hyprix compositing in software):
* hyprix maps the **dmabuf fd** itself (Linux: `dma_buf_mmap` → `drm_gem_prime_mmap` → the GEM's `.mmap`).
* So the Ferrix dmabuf object must be mappable. No NVIDIA ioctl is involved on hyprix's side.
* Chrome's own `gbm_bo_map` path uses GEM_MAP_OFFSET plus render-node mmap.
* `DMA_BUF_IOCTL_SYNC` (0x40086200) should be accepted as a no-op. The memory is coherent: write-back, snooped.

**(d) Sync:**
* **Implicit sync:** none needed for step 1, if Chrome CPU-waits its fence before committing ([src-knowledge], verify).
* **Explicit sync** (`wp_linux_drm_syncobj_v1`, later):
  * SYNCOBJ_CREATE, DESTROY, HANDLE_TO_FD, FD_TO_HANDLE (incl. sync-file import/export flags), TIMELINE_WAIT, TRANSFER, QUERY;
  * GET_CAP SYNCOBJ/TIMELINE;
  * GET_DEV_INFO `supports_sync_fd/semsurf=1`;
  * SEMSURF_FENCE_CTX_CREATE, CREATE, WAIT, ATTACH;
  * a kernel sync_file object;
  * and on hyprix's side, a CPU wait on a syncobj point (eventfd or wait).

---

## 3. How NVIDIA memory becomes a dma-buf on Linux

### 3.1 nvidia-drm GEM objects (the path GBM and EGL use)

* **Export** is `drm_gem_prime_export` with DRM core's `drm_gem_prime_dmabuf_ops`. These call back into the driver:
  * `.get_sg_table` = `nv_drm_gem_prime_get_sg_table` (`gem.c:178-187`) → `__nv_drm_gem_nvkms_memory_prime_get_sg_table` (`gem-nvkms-memory.c:244-264`). It builds the sg_table **only from sysmem pages**: `pages_count == 0` (vidmem) → `-ENOMEM`. So **a vidmem GEM cannot be attached by another device**.
  * `.vmap`: sysmem `vmap` of pages; vidmem `ioremap_wc` of the BAR1 mapping (`:194-221`).
  * `.mmap`: `drm_gem_prime_mmap` → `__nv_drm_gem_nvkms_mmap` and fault handler (`:70-140`). Sysmem maps the page pfns; vidmem maps the BAR1 range from `nvKms->mapMemory(USER)`.
* **Same-GPU NVIDIA importers never use those ops.** The core import returns the same GEM object, and userspace then moves the *RM object* across with EXPORT_NVKMS_MEMORY and RM's fd export/import.
  * So for NVIDIA↔NVIDIA, **the dma-buf is only a token naming a GEM object (RM memory)**.
* **Pages only matter for CPU mmap** (hyprix in software) **and for foreign importers.**

### 3.2 `kernel-open/nvidia/nv-dmabuf.c` (`NV_ESC_EXPORT_TO_DMABUF_FD`, 0xCA3046D9)

* This is RM's own dma-buf.
* **Params** (`nv-ioctl.h:134-148`): `{fd (-1 to create, or an existing fd to add to), hClient, totalObjects, numObjects, index, totalSize, mappingType (DEFAULT / FORCE_PCIE), bAllowMmap, handles[128], offsets[128], sizes[128]}`.
* **Create** (`nv-dmabuf.c:1470-1600`, entry `nv_dma_buf_export` `:1732`):
  * `rm_dma_buf_get_client_and_device`;
  * `nv_dma_buf_dup_mem_handles` → `rm_dma_buf_dup_mem_handle` (`:373-470`) dups each RM handle into an internal client and records `can_mmap`, cache type and memory type;
  * `static_phys_addrs` platforms prefetch the addresses;
  * then `dma_buf_export(&nv_dma_buf_ops)` and `dma_buf_fd`.
* **Ops** (`:1454-1468`): `attach` (PCI topology checks, `:1001-1063`), `map_dma_buf` (`:1065-1130`), `unmap_dma_buf`, `release`, `mmap` (`:1223-`, walks `memArea.pRanges`, allowed only with `bAllowMmap`). `map_dma_buf`:
  * calls `rm_dma_buf_map_mem_handle`, which gives RM's physical ranges: **BAR1 addresses for vidmem**, or sysmem pages;
  * then `nv_dma_buf_map_pages` (struct-page sysmem, coherent platforms) or `nv_dma_buf_map_pfns` (MMIO/BAR1 as peer DMA addresses).
* **Who uses it.** It is CUDA/RDMA's export (`cuMemExportToShareableHandle`, GPUDirect), and it appears in the allocator, glcore, eglcore and glsi. **For a dGPU with nvidia-drm's GBM path it is not the route, since `nv_gbm.c` uses ALLOC_NVKMS + PRIME.**
* nvrm answers `-EINVAL` today (`os/kept/nv.c:818-823`), and `os_dma_buf_enabled = 0` (`os/nvos/src/os.rs:52-54`).

### 3.3 What a Ferrix kernel "dmabuf" object must carry

* **For hyprix's CPU mmap:** a **VMO range** (`vmo, offset, length`). Sysmem RM allocations in nvrm already *are* VMOs: `os/nvos/src/pages.rs:1-40` ("an allocation is a VMO, pinned into the card's IOMMU domain"). `os/glue/chardev.c:187-200` already answers mmap with `nvos_pages_vmo` + `NVOS_MAP_VMO`.
  * The kernel's mmap path already takes any inode whose `mapping_at()` returns a `Vmo` (`src/kernel/src/syscall/memory.rs:368-394`), or a window (`map_window`, `:404-`).
  * So a dmabuf inode with `mapping_at → (Arc<Vmo>, offset)` maps with **no new mm code**.
* **For vidmem:** an aperture range (a BAR1 window, WC). It is mappable, but CPU reads over BAR1 WC are very slow, so it is unusable for CPU compositing. It is useful only as a token for N3c and NVIDIA importers.
* **For another NVIDIA client to import:** an **exporter identity**: the nvrm control plus nvrm's 64-bit cookie for the GEM object. With it, PRIME_FD_TO_HANDLE on renderD129 can give nvrm the cookie, and nvrm hands back the same GEM object, as Linux does. No pages are needed for this.
* **For a foreign importer** (none in N3b): the VMO, which nvrm could wrap as an RM OS descriptor (`NVOS32_DESCRIPTOR_TYPE_OS_SGT_PTR`-like, over pinned pages). That is later.
* **Lifetime:** the dmabuf holds a reference on the VMO (and so on the pages). nvrm must keep the RM memory, and the VMO pin, until the last dmabuf referencing the cookie is gone. That needs a release notice from the kernel to nvrm, or nvrm only drops the GEM when both the GEM handles *and* the exported dmabufs are gone. The kernel already notifies RELEASE for files; a dmabuf release would be a new message.
* **Also:** `poll` always ready (no implicit fences yet); `DMA_BUF_IOCTL_SYNC` no-op success; `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` → an already-signalled sync_file, or ENOTTY (Chrome and egl-wayland do not need it); `stat`/fdinfo as `anon_inode:[dmabuf]`; passable over SCM_RIGHTS (any file is, `fs/socket.rs`).

---

## 4. What exists in Ferrix today

(From a code-reading subagent, spot-checked.)

* **The render core is ring-3-driven and numbered.**
  * `render_control_create` plus HELLO over `renderctl` (`src/kernel/src/interfaces/render/mod.rs:515-560`, `677-766`).
  * Numbers come from `Numbers::new(128)`, first come first served (`mod.rs:494-496`).
  * Its other pieces: the `RENDERERS` registry (`:499`, `:612`); devfs `/dev/dri/renderD<N>` (`fs/devfs.rs:943-948`, `1054-1061`, `1349-1355`); `stat` 226:N (`mod.rs:811-834`); sysfs `/sys/class/drm/renderD<N>` and `/sys/dev/char/226:N` (`fs/sysfs.rs:807-811`, `1294`, `1375`, `1466`).
  * But a renderer must speak `renderctl`, which is virtio-shaped. Ioctl decoding is virtgpu-specific (`render/node.rs:459-488`): VERSION, the VIRTGPU_* set, GET_CAP (only SYNCOBJ/TIMELINE, as 0, `node.rs:852-863`), GEM_CLOSE and PRIME_HANDLE_TO_FD. **No PRIME_FD_TO_HANDLE on the render node, no SYNCOBJ_\*.**
  * VERSION's name is the driver's HELLO name, but the description is hardcoded `virtio GPU` (`node.rs:1057-1082`).
* **PRIME today.**
  * `PRIME_HANDLE_TO_FD` wraps the object in `Exported` (`node.rs:944-1019`) via `fs::anon::open`, named `anon_inode:[dmabuf]`. `Exported` is documented as **not mappable, not for another device** (`node.rs:947-952`). It has no `mapping_at`, so mmap is ENODEV (`syscall/memory.rs:369-373`). No poll and no `DMA_BUF_IOCTL_*`.
  * The card's `PRIME_FD_TO_HANDLE` (`interfaces/display/drm.rs:1073-1142`) downcasts with `render::node::exported()` and calls `card.attach_object` (hyprix's virgl frame → scanout).
  * **There is no general dmabuf object type** in `src/kernel/src/object` or `fs`.
* **Chardev core** (`src/kernel/src/interfaces/chardev/mod.rs`, protocol `src/lib/proto/chardevctl/src/message.rs`):
  * Open, Ioctl, Mmap and Release are forwarded undecoded.
  * Native calls: `chardev_reply`, `chardev_copy_in/out` (≤1 MiB) and `chardev_file(control, request, fd)` (`mod.rs:866-883`). The last resolves the client's fd to *this control's* file identity; it backs `nv_get_file_private` (`nvrm os/nvos/src/client.rs:81-92`).
  * Mmap replies: `MAP_VMO`=1 (a VMO handle from the driver's table, plus offset, rights-checked), `MAP_APERTURE`=2 (a range in the device's BARs), `MAP_WRITE_COMBINING` (prefetchable only) (`mod.rs:806-862`, `message.rs:50-58`).
  * **There is no way for a driver to create an fd in the client.** An Ioctl reply is an i32 value (`mod.rs:785-791`).
  * Node names are fixed to major 195 (`nvidia<N>`, `nvidiactl`, `nvidia-modeset`, `chardevctl/src/node.rs:1-77`). There is **no `/dev/dri` path**, and poll is never ready (`chardev/file.rs`).
* **hyprix:**
  * No linux-dmabuf, syncobj or wl_drm XML in `compositor/protocol/protocols/` and no code. Clients are `wl_shm` only (`hyprix/src/state.rs:40`, `3733`, `3929`).
  * Its GPU renderer hardcodes `/dev/dri/renderD128` (`compositor/drm/src/render.rs:37`) and requires driver `virtio_gpu` (`state.rs:3052-3062`).
  * It exports its own frame by PRIME to the card (`render.rs:403-419`, `card.rs:826-871`).
* **nvrm:**
  * RM and NVKMS are in one process.
  * KAPI client `os/glue/kms.c` (it has the device resource info GET_DEV_INFO needs).
  * Sysmem allocations are VMOs (`pages.rs`); mmap of a control file's sysmem uses MAP_VMO (`chardev.c:187-200`).
  * `NV_ESC_EXPORT_TO_DMABUF_FD` → `-EINVAL`; `nv_dma_import_*` are stubs (`os/glue/stubs.c:215-243`).

---

## 5. Recommended minimal path

**Principle.** Reuse the render core for the **name** (number, devfs, sysfs, 226:N: what libdrm's `drmGetDevice*`, Chrome's `main_device` lookup and the Vulkan driver's `/dev/dri` scan all need). Reuse the chardev core for **transport**: nvrm decodes every DRM ioctl itself, as it does RM's. Add one kernel object, a VMO-backed dmabuf. Put buffers in **system memory, pitch-linear**.

### Step 1: the kernel (three small mechanisms)

1. **A chardev-served render node.**
   * Let a chardev control register a node of kind "render". The kernel takes a number from the render core's `NUMBERS` and adds it to the registry that devfs/sysfs read (`RENDERERS`, or a parallel list both consult), so `/dev/dri/renderD129`, `stat` 226:129 and `/sys/dev/char/226:129` → the 3060's PCI device all exist.
   * Opens, ioctls, mmaps and releases on it are forwarded to nvrm **on the same control as `/dev/nvidiactl`**. This matters: GEM_IMPORT/EXPORT_NVKMS_MEMORY carry a client nvidiactl `memFd` that RM resolves with `chardev_file`, which only knows that control's files.
   * **Stable numbering.** virtio-gpu must keep renderD128. Either let a driver ask for a floor (≥129), or make hyprix pick its node by driver name rather than the hardcoded `renderD128` (`render.rs:37`).
2. **A dmabuf object.** An anon inode `anon_inode:[dmabuf]` holding:
   * `Arc<Vmo>`, offset and size (or an aperture range, for vidmem tokens);
   * `exporter: (control, cookie)`.
   * Behaviour: `mapping_at → (vmo, offset+off)` (mmap works with today's code); `poll` ready; `DMA_BUF_IOCTL_SYNC` → 0; other ioctls ENOTTY.
   * When the last reference goes, it sends nvrm a `DMABUF_RELEASE(cookie)` message.
   * Later, the existing `Exported` (virtio) could become the same type.
3. **Two native calls**, valid only while a request is outstanding, like `chardev_file`:
   * `chardev_dmabuf_install(request, vmo_handle, offset, size, cookie, flags) -> fd`. It installs the fd in the waiting client (O_CLOEXEC/O_RDWR as asked). nvrm writes the fd into the PRIME_HANDLE_TO_FD reply. Dedup is per cookie: the kernel returns the existing dmabuf's new fd if one is alive, as Linux re-uses `obj->dma_buf`.
   * `chardev_dmabuf_resolve(request, fd) -> cookie`, or `-EXDEV` for a dmabuf from another exporter (later: hand nvrm the VMO for an RM OS descriptor), or `-EINVAL`.
   * The same install/resolve pair, generalised to "a file object of kind K", serves sync_files and syncobj fds later.

### Step 2: nvrm serves renderD129 (a nvidia-drm subset written over KAPI)

* **A GEM table per open file:** handle → object {`NvKmsKapiMemory*`, size, isVidmem, VMO + offset for sysmem, cookie}. Objects are refcounted across files, through PRIME, and by live dmabufs.
* **Ioctls:**
  * VERSION: `nvidia-drm`, 0.0.0, `20160202`, `NVIDIA DRM driver`.
  * GET_CAP: PRIME=3; SYNCOBJ=0 and TIMELINE=0 for now; others EINVAL.
  * GET_DEV_INFO: from KAPI resource info; `supports_alloc=1`, `supports_sync_fd=0`, `supports_semsurf=0` for now; `primary_index` = nvrm's display card index.
  * DMABUF_SUPPORTED → 0.
  * GET_DRM_FILE_UNIQUE_ID.
  * GEM_ALLOC_NVKMS_MEMORY.
  * GEM_MAP_OFFSET, plus mmap of the node by offset (MAP_VMO; MAP_APERTURE+WC for vidmem).
  * GEM_IMPORT_NVKMS_MEMORY and GEM_EXPORT_NVKMS_MEMORY (`nvKms->importMemory/exportMemory`, with the memFd resolved through the existing `resolve_fd`).
  * GEM_IDENTIFY_OBJECT.
  * GEM_CLOSE.
  * PRIME_HANDLE_TO_FD (`chardev_dmabuf_install`).
  * PRIME_FD_TO_HANDLE (`chardev_dmabuf_resolve` → same object → new or existing handle in this file).
  * GET_CLIENT_CAPABILITY: EACCES on a render node, as Linux.
  * Everything else (fences, semsurf, syncobj, CRC, permissions): EINVAL or ENOTTY for now. That is safe because `supports_sync_fd/semsurf=0` and GET_CAP SYNCOBJ=0.
* **Placement policy:**
  * `block_linear == 0` → **sysmem** (`useVideoMemory=false`): write-back, coherent, one VMO per RM allocation.
  * `block_linear == 1` → vidmem (an NVIDIA-only token; mmap via BAR1 WC).
* **Finding the VMO for a KAPI allocation.** KAPI hands back an RM handle, not nvos pages. Resolve the backing `nv_alloc_t` → `nvos_pages` → `nvos_pages_vmo` (as `chardev.c:194` does), for example by matching `getMemoryPages`' first address against nvrm's allocation list, or by adding a small hook in `os/kept`.

### Step 3: hyprix gains `zwp_linux_dmabuf_v1` (v4), CPU-composited

* Advertise ARGB8888 and XRGB8888 (ABGR/XBGR too) with **`DRM_FORMAT_MOD_LINEAR` only**.
* Feedback `main_device` = 226:129, the tranche target device the same.
* The format table in a sealed memfd (memfd exists, `syscall/memfd.rs`).
* `create_params`/`add`/`create_immed`: one plane, take fd, offset, stride and modifier; `mmap(PROT_READ, MAP_SHARED)` the fd; treat it like an shm buffer with a stride; send `wl_buffer.release` after the frame that read it.
* Optionally, `DMA_BUF_IOCTL_SYNC` around reads.
* No `wl_drm`, no syncobj yet.

### Step 4: launch

* Drop `--disable-gpu-compositing`; add `--render-node-override=/dev/dri/renderD129`; set `GBM_BACKENDS_PATH` to the volume's `usr/lib/x86_64-linux-gnu/gbm` (and `GBM_BACKEND=nvidia-drm` as a belt).
* Chrome's own `libgbm.so.1` (Mesa 25.0.7, in the Chrome tree) loads `nvidia-drm_gbm.so`, which `dlopen`s `libdrm.so.2` (present in both trees).
* Verify, in this order:
  1. `chrome://gpu` shows "Compositing: Hardware accelerated";
  2. nvrm logs ALLOC_NVKMS, PRIME export/import and EXPORT_NVKMS;
  3. hyprix shows frames with no tearing;
  4. fps against the 45 fps baseline.
* A tiny test client on the guest is worth writing first: `gbm_create_device(renderD129)` → `gbm_bo_create_with_modifiers(LINEAR)` → export fd → mmap → write → import into Vulkan via `VK_EXT_external_memory_dma_buf`. It tests step 1–2 without Chrome.

### Step 5 (later): explicit sync, then N3c

* SYNCOBJ_* plus a sync_file object in the kernel, SEMSURF in nvrm (`supports_sync_fd=1`), `wp_linux_drm_syncobj_v1` in hyprix.
* Then N3c: hyprix's renderer on NVIDIA Vulkan imports client dmabufs by PRIME_FD_TO_HANDLE (the same-device cookie path, no copy), and vidmem block-linear modifiers can be advertised.

### Risks

1. **The bottleneck may move to hyprix.** Chrome's readback and its own software compositing go away, but hyprix still composites on the CPU (39 ms per full 1080p frame, docs/NVIDIA.md §4.6, docs/GPU.md). The fps gain may be partial until N3c; damage-limited compositing helps.
2. **Linear sysmem render targets** cost GPU bandwidth over PCIe. About 8 MB per 1080p frame is fine. But **check that NVIDIA Vulkan exposes LINEAR + COLOR_ATTACHMENT** for the import (`vkGetPhysicalDeviceImageFormatProperties2` with a DRM modifier). If not, ANGLE refuses the import and Chrome falls back.
3. **Sync assumption.** Step 1 relies on Chrome CPU-waiting its fence before commit ([src-knowledge]). If frames tear, explicit sync (step 5) moves up.
4. **Unverified call order inside the closed libraries** (the import path via EXPORT_NVKMS + RM fd import, the allocator's device scan of `card%d`/`renderD`, `primary_index` use). Log every unhandled ioctl number in nvrm on first runs.
5. **Device discovery:**
   * libdrm's `drmGetDevice*` (egl-wayland, Chrome's feedback path) read `/sys/dev/char/226:N/device/…` and the PCI ids. That works only if the node is in the render core's sysfs registry.
   * GET_DEV_INFO `primary_index` must name a `card%d` the libraries can open or `stat`. nvrm's display card exists (N6a), but its index depends on registration order.
6. **Numbering collision.** First-come numbering can swap renderD128 and renderD129 and break hyprix's hardcoded node.
7. **Lifetimes.** GEM objects must outlive exported dmabufs (DMABUF_RELEASE). A dmabuf holding the VMO keeps nvrm's pin alive against the per-device pin budget (N0f). A crashed nvrm leaves dmabufs whose VMOs stay valid memory, which is safe, but whose cookies are dead: import must fail cleanly after a restart.
8. **The certified item.** The render, chardev and fs changes are kernel changes in the `load` ring. They need the certification consultant's review before landing (docs/NVIDIA.md §6).
9. **ABI between libgbm 25.0.7 and NVIDIA 580's backend.** It should be fine (Mesa keeps old backend ABIs), but it is untested here.
10. **`NV_ESC_EXPORT_TO_DMABUF_FD`** appears in the allocator and GL libraries. If a path hits it, `-EINVAL` must not be fatal. Watch the logs.
