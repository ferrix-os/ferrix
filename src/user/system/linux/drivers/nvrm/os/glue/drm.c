/*
 * nvidia-drm's render node, the part NVIDIA's GBM backend, EGL and Vulkan
 * use (N3b, docs/NVIDIA.md §4.6): GEM objects over NVKMS memory, PRIME
 * through the kernel's dmabuf object, and NVIDIA's private ioctls that move
 * an object between a GEM handle and a client's own RM objects. Written
 * here over KAPI rather than ported from nvidia-drm's Linux DRM code; the
 * ioctls and their answers follow kernel-open/nvidia-drm.
 *
 * The kernel publishes the node as /dev/dri/renderD<N> and forwards its
 * opens, ioctls, mmaps and releases on the chardev core's RENDER_MINOR,
 * beside /dev/nvidiactl's, so an ioctl that names a client's nvidiactl
 * descriptor (EXPORT/IMPORT_NVKMS_MEMORY's memFd) is resolved as RM's own
 * are.
 *
 * Placement is this file's policy: a pitch-linear buffer is allocated in
 * system memory, where it is one of nvrm's VMOs, so that its dmabuf can be
 * mapped by the compositor, which composites on the processor. A
 * block-linear buffer stays in video memory; its dmabuf is name-only (no
 * VMO, unmappable), which NVIDIA's userspace in another process imports
 * again through PRIME_FD_TO_HANDLE (docs/NVIDIA.md §4.4, N3b). No synchronisation is offered yet: GET_DEV_INFO says
 * supports_sync_fd and supports_semsurf 0, and GET_CAP says no syncobj.
 *
 * A GEM object lives while a handle names it or a dmabuf of it is open;
 * the kernel says when the last descriptor of a dmabuf goes (a
 * DMABUF_RELEASE request naming the object's cookie).
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"
#include "nvidia-modeset-os-interface.h"
#include "nvkms.h"

const struct NvKmsKapiFunctionsTable *nvrm_kms_kapi(void);
struct NvKmsKapiDevice *nvrm_kms_device(NvU32 *gpu_id, NvU32 *page_kind);

#define drm_say(...)   nv_printf(NV_DBG_ERRORS, "nvrm: drm: " __VA_ARGS__)

#ifndef EBADF
#define EBADF   9
#endif
#ifndef ENOENT
#define ENOENT  2
#endif
#ifndef ENOSPC
#define ENOSPC  28
#endif
#ifndef EOPNOTSUPP
#define EOPNOTSUPP 95
#endif

/* DRM's core ioctls this node answers (include/uapi/drm/drm.h). */
#define DRM_IOCTL_VERSION               0xC0406400u
#define DRM_IOCTL_GEM_CLOSE             0x40086409u
#define DRM_IOCTL_GET_CAP               0xC010640Cu
#define DRM_IOCTL_SET_CLIENT_CAP        0x4010640Du
#define DRM_IOCTL_PRIME_HANDLE_TO_FD    0xC00C642Du
#define DRM_IOCTL_PRIME_FD_TO_HANDLE    0xC00C642Eu

/* NVIDIA's (kernel-open/nvidia-drm/nvidia-drm-ioctl.h). */
#define DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY    0xC0206441u
#define DRM_IOCTL_NVIDIA_GET_DEV_INFO               0xC0246443u
#define DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY    0xC0186449u
#define DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET             0xC010644Au
#define DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY     0xC018644Bu
#define DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT        0xC008644Eu
#define DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED           0x0000644Fu
#define DRM_IOCTL_NVIDIA_GET_DRM_FILE_UNIQUE_ID     0xC0086458u
#define DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CTX_CREATE   0xC0206454u
#define DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE       0xC0186455u

#define DRM_CAP_PRIME                   0x5
#define DRM_PRIME_CAP_IMPORT            0x1
#define DRM_PRIME_CAP_EXPORT            0x2
#define DRM_CLOEXEC                     0x80000u   /* O_CLOEXEC */
#define DRM_RDWR                        0x2u       /* O_RDWR */
#define NV_GEM_ALLOC_NO_SCANOUT         (1u << 0)

struct drm_version {
    NvS32 version_major;
    NvS32 version_minor;
    NvS32 version_patchlevel;
    NvU32 pad;
    NvU64 name_len;
    NvU64 name;
    NvU64 date_len;
    NvU64 date;
    NvU64 desc_len;
    NvU64 desc;
};

struct drm_get_cap {
    NvU64 capability;
    NvU64 value;
};

struct drm_gem_close {
    NvU32 handle;
    NvU32 pad;
};

struct drm_prime_handle {
    NvU32 handle;
    NvU32 flags;
    NvS32 fd;
};

struct drm_nvidia_gem_import_nvkms_memory_params {
    NvU64 mem_size;
    NvU64 nvkms_params_ptr;
    NvU64 nvkms_params_size;
    NvU32 handle;
    NvU32 pad;
};

struct drm_nvidia_get_dev_info_params {
    NvU32 gpu_id;
    NvU32 mig_device;
    NvU32 primary_index;
    NvU32 supports_alloc;
    NvU32 generic_page_kind;
    NvU32 page_kind_generation;
    NvU32 sector_layout;
    NvU32 supports_sync_fd;
    NvU32 supports_semsurf;
};

struct drm_nvidia_gem_export_nvkms_memory_params {
    NvU32 handle;
    NvU32 pad;
    NvU64 nvkms_params_ptr;
    NvU64 nvkms_params_size;
};

struct drm_nvidia_gem_map_offset_params {
    NvU32 handle;
    NvU32 pad;
    NvU64 offset;
};

struct drm_nvidia_gem_alloc_nvkms_memory_params {
    NvU32 handle;
    NvU8 block_linear;
    NvU8 compressible;
    NvU16 pad0;
    NvU64 memory_size;
    NvU32 flags;
    NvU32 pad1;
};

struct drm_nvidia_semsurf_fence_ctx_create_params {
    NvU64 index;
    NvU64 nvkms_params_ptr;
    NvU64 nvkms_params_size;
    NvU32 handle;
    NvU32 pad;
};

struct drm_nvidia_semsurf_fence_create_params {
    NvU32 fence_context_handle;
    NvU32 timeout_value_ms;
    NvU64 wait_value;
    NvS32 fd;
    NvU32 pad;
};

struct drm_nvidia_gem_identify_object_params {
    NvU32 handle;
    NvU32 object_type;      /* 0 NVKMS, 1 DMABUF, 2 USERMEMORY */
};

/* ------------------------------------------------------------------------
 * GEM objects, by cookie, shared by every open of the node: a dmabuf names
 * an object by its cookie, which is the same in every file.
 * ---------------------------------------------------------------------- */

struct drm_gem {
    struct drm_gem *next;
    NvU64 cookie;
    NvU64 size;
    struct NvKmsKapiMemory *memory;
    /*
     * Or, for a fence context (SEMSURF_FENCE_CTX_CREATE), no memory: the
     * client's semaphore surface, imported, and the index of its semaphore.
     */
    struct NvKmsKapiSemaphoreSurface *semsurf;
    NvU64 sem_index;
    void *sem_map;
    /* Fences on it still waiting for their callback: it outlives them. */
    NvU32 fences;
    /* The system-memory allocation it is, or NULL in video memory. */
    nv_alloc_t *at;
    /*
     * Handles naming it, in every file, and its dmabufs alive: one up for
     * each the kernel says an install made, one down for each release, so
     * a release of a dmabuf that went just before another was made is not
     * read as the release of the new one.
     */
    NvU32 handles;
    NvU32 dmabufs;
};

static struct drm_gem *gems;
static nvos_mutex_t gems_lock;
static NvU64 next_cookie = 1;

/* One open of the node. */
struct drm_file {
    NvU64 unique;
    nvos_mutex_t lock;
    /* Handle i + 1 names table[i]; NULL is a free handle. */
    struct drm_gem **table;
    NvU32 room;
};

static NvU64 next_unique = 1;

static struct drm_gem *gem_by_cookie_locked(NvU64 cookie)
{
    struct drm_gem *gem;

    for (gem = gems; gem != NULL; gem = gem->next)
        if (gem->cookie == cookie)
            return gem;
    return NULL;
}

/*
 * gems_lock is a leaf: nothing is called under it that can wait. KAPI's
 * frees take RM's locks, and KAPI's semaphore-surface callback
 * (drm_fence_reached) takes gems_lock from inside RM, so a free made under
 * gems_lock and a fence reached at the same moment would each wait for what
 * the other holds. The dispatch thread takes gems_lock too, for every open,
 * mmap and release of the node, and would stop behind them.
 *
 * So an object is ended in two steps: gem_unlink_locked takes it out of the
 * list under the lock, once nothing names it, and gem_free gives its memory
 * back to KAPI after the lock is dropped. Unlinked, no cookie, handle or
 * fence can find it, so gem_free's caller is its only holder.
 */

/* Unlink `gem` if nothing names it any more; under gems_lock. Whether the
 * caller must gem_free it once the lock is dropped. */
static NvBool gem_unlink_locked(struct drm_gem *gem)
{
    struct drm_gem **link;

    if (gem->handles != 0 || gem->dmabufs != 0 || gem->fences != 0)
        return NV_FALSE;
    for (link = &gems; *link != NULL; link = &(*link)->next)
    {
        if (*link == gem)
        {
            *link = gem->next;
            gem->next = NULL;
            return NV_TRUE;
        }
    }
    /* Not in the list: another thread unlinked it and frees it. */
    return NV_FALSE;
}

/* Give an unlinked object's memory back; never under gems_lock. */
static void gem_free(struct drm_gem *gem)
{
    NvU32 gpu_id, kind;

    if (gem->memory != NULL)
        nvrm_kms_kapi()->freeMemory(nvrm_kms_device(&gpu_id, &kind), gem->memory);
    if (gem->semsurf != NULL)
        nvrm_kms_kapi()->freeSemaphoreSurface(nvrm_kms_device(&gpu_id, &kind), gem->semsurf);
    free(gem);
}

/* End `gem` if nothing names it any more: what a caller that holds no lock
 * does after taking a name away. */
static void gem_settle(struct drm_gem *gem)
{
    NvBool dead;

    nvos_mutex_lock(&gems_lock);
    dead = gem_unlink_locked(gem);
    nvos_mutex_unlock(&gems_lock);
    if (dead)
        gem_free(gem);
}

/*
 * A GEM object over `memory`, with the allocation it is when it is in
 * system memory. Linked, with no handle yet.
 */
static struct drm_gem *gem_new(struct NvKmsKapiMemory *memory, NvU64 size)
{
    const struct NvKmsKapiFunctionsTable *kapi = nvrm_kms_kapi();
    NvU32 gpu_id, kind;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);
    struct drm_gem *gem = calloc(1, sizeof(*gem));
    NvU64 *pages = NULL;
    NvU32 count = 0;

    if (gem == NULL)
        return NULL;
    gem->memory = memory;
    gem->size = size;
    if (memory != NULL && !kapi->isVidmem(memory) && kapi->getMemoryPages(device, memory, &pages, &count))
    {
        if (count > 0)
            gem->at = nvrm_alloc_at(pages[0]);
        kapi->freeMemoryPages(pages);
    }
    nvos_mutex_lock(&gems_lock);
    gem->cookie = next_cookie++;
    gem->next = gems;
    gems = gem;
    nvos_mutex_unlock(&gems_lock);
    return gem;
}

/* A new handle in `file` for `gem`, which takes a reference; 0 if none. */
static NvU32 handle_new(struct drm_file *file, struct drm_gem *gem)
{
    NvU32 i, handle = 0;

    nvos_mutex_lock(&file->lock);
    for (i = 0; i < file->room && file->table[i] != NULL; i++)
        ;
    if (i == file->room)
    {
        NvU32 room = file->room ? file->room * 2 : 16;
        struct drm_gem **grown = calloc(room, sizeof(*grown));
        if (grown != NULL)
        {
            if (file->table != NULL)
                memcpy(grown, file->table, file->room * sizeof(*grown));
            free(file->table);
            file->table = grown;
            file->room = room;
        }
    }
    if (i < file->room)
    {
        file->table[i] = gem;
        handle = i + 1;
        nvos_mutex_lock(&gems_lock);
        gem->handles++;
        nvos_mutex_unlock(&gems_lock);
    }
    nvos_mutex_unlock(&file->lock);
    return handle;
}

/* The object `handle` names in `file`, or NULL. */
static struct drm_gem *handle_gem(struct drm_file *file, NvU32 handle)
{
    struct drm_gem *gem = NULL;

    nvos_mutex_lock(&file->lock);
    if (handle != 0 && handle <= file->room)
        gem = file->table[handle - 1];
    nvos_mutex_unlock(&file->lock);
    return gem;
}

/* The handle naming `gem` in `file` already, or 0. */
static NvU32 handle_of(struct drm_file *file, const struct drm_gem *gem)
{
    NvU32 i, handle = 0;

    nvos_mutex_lock(&file->lock);
    for (i = 0; i < file->room; i++)
    {
        if (file->table[i] == gem)
        {
            handle = i + 1;
            break;
        }
    }
    nvos_mutex_unlock(&file->lock);
    return handle;
}

static int handle_close(struct drm_file *file, NvU32 handle)
{
    struct drm_gem *gem = NULL;
    NvBool dead;

    nvos_mutex_lock(&file->lock);
    if (handle != 0 && handle <= file->room)
    {
        gem = file->table[handle - 1];
        file->table[handle - 1] = NULL;
    }
    nvos_mutex_unlock(&file->lock);
    if (gem == NULL)
        return -EINVAL;
    nvos_mutex_lock(&gems_lock);
    gem->handles--;
    dead = gem_unlink_locked(gem);
    nvos_mutex_unlock(&gems_lock);
    if (dead)
        gem_free(gem);
    return 0;
}

/* ------------------------------------------------------------------------
 * Opens and closes.
 * ---------------------------------------------------------------------- */

void *nvrm_drm_open(void)
{
    struct drm_file *file = calloc(1, sizeof(*file));

    if (file == NULL)
        return NULL;
    nvos_mutex_lock(&gems_lock);
    file->unique = next_unique++;
    nvos_mutex_unlock(&gems_lock);
    return file;
}

void nvrm_drm_close(void *opened)
{
    struct drm_file *file = opened;
    NvU32 i;

    if (file == NULL)
        return;
    for (i = 0; i < file->room; i++)
        if (file->table[i] != NULL)
            (void)handle_close(file, i + 1);
    free(file->table);
    free(file);
}

void nvrm_drm_released(NvU64 cookie)
{
    struct drm_gem *gem;
    NvBool dead = NV_FALSE;

    nvos_mutex_lock(&gems_lock);
    gem = gem_by_cookie_locked(cookie);
    if (gem != NULL && gem->dmabufs != 0)
    {
        gem->dmabufs--;
        dead = gem_unlink_locked(gem);
    }
    nvos_mutex_unlock(&gems_lock);
    if (dead)
        gem_free(gem);
}

/* ------------------------------------------------------------------------
 * The ioctls. Each runs on the requesting client's behalf: its arguments
 * are copied with RM's user copies, which reach that client.
 * ---------------------------------------------------------------------- */

static int copy_in(void *to, NvU64 from, NvU32 length)
{
    return os_memcpy_from_user(to, (const void *)(NvUPtr)from, length) == NV_OK ? 0 : -EFAULT;
}

static int copy_out(NvU64 to, const void *from, NvU32 length)
{
    return os_memcpy_to_user((void *)(NvUPtr)to, from, length) == NV_OK ? 0 : -EFAULT;
}

/* One string of VERSION's: as much as fits, and its whole length. */
static int version_string(NvU64 to, NvU64 *len, const char *text)
{
    NvU64 whole = strlen(text);

    if (to != 0 && *len > 0 && copy_out(to, text, (NvU32)(*len < whole ? *len : whole)) != 0)
        return -EFAULT;
    *len = whole;
    return 0;
}

static int drm_version(NvU64 arg)
{
    struct drm_version v;

    if (copy_in(&v, arg, sizeof(v)) != 0)
        return -EFAULT;
    v.version_major = 0;
    v.version_minor = 0;
    v.version_patchlevel = 0;
    /* What libgbm picks NVIDIA's backend by: gbm/nvidia-drm_gbm.so. */
    if (version_string(v.name, &v.name_len, "nvidia-drm") != 0 ||
        version_string(v.date, &v.date_len, "20160202") != 0 ||
        version_string(v.desc, &v.desc_len, "NVIDIA DRM driver") != 0)
        return -EFAULT;
    return copy_out(arg, &v, sizeof(v));
}

static int drm_get_cap(NvU64 arg)
{
    struct drm_get_cap cap;

    if (copy_in(&cap, arg, sizeof(cap)) != 0)
        return -EFAULT;
    switch (cap.capability)
    {
    case DRM_CAP_PRIME:
        cap.value = DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT;
        break;
    default:
        /* No syncobj, no timeline, no dumb buffers on a render node. */
        cap.value = 0;
        break;
    }
    return copy_out(arg, &cap, sizeof(cap));
}

static int drm_dev_info(NvU64 arg)
{
    struct drm_nvidia_get_dev_info_params p;
    NvU32 gpu_id = 0, kind = 0;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);

    memset(&p, 0, sizeof(p));
    p.gpu_id = gpu_id;
    /* card0: the display card nvrm serves (os/glue/kms.c). */
    p.primary_index = 0;
    if (device != NULL)
    {
        p.supports_alloc = 1;
        p.generic_page_kind = kind;
        /* As nvidia-drm: Turing and later are generation 2. */
        p.page_kind_generation = (kind == 0x06) ? 2 : 0;
        p.sector_layout = 1;
        /* EXPERIMENT (N3b sync): what Chrome reaches once these say yes. */
        p.supports_sync_fd = 1;
        p.supports_semsurf = 1;
    }
    return copy_out(arg, &p, sizeof(p));
}

static int drm_alloc(struct drm_file *file, NvU64 arg)
{
    struct drm_nvidia_gem_alloc_nvkms_memory_params p;
    struct NvKmsKapiAllocateMemoryParams params;
    const struct NvKmsKapiFunctionsTable *kapi = nvrm_kms_kapi();
    NvU32 gpu_id, kind;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);
    struct NvKmsKapiMemory *memory;
    struct drm_gem *gem;

    if (device == NULL)
        return -EOPNOTSUPP;
    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    if (p.pad0 != 0 || p.pad1 != 0 || p.memory_size == 0)
        return -EINVAL;

    memset(&params, 0, sizeof(params));
    params.layout = p.block_linear ? NvKmsSurfaceMemoryLayoutBlockLinear
                                   : NvKmsSurfaceMemoryLayoutPitch;
    params.type = (p.flags & NV_GEM_ALLOC_NO_SCANOUT) ? NVKMS_KAPI_ALLOCATION_TYPE_OFFSCREEN
                                                      : NVKMS_KAPI_ALLOCATION_TYPE_SCANOUT;
    params.size = p.memory_size;
    /* Pitch-linear in system memory, where the compositor can map it. */
    params.useVideoMemory = p.block_linear ? NV_TRUE : NV_FALSE;
    params.compressible = &p.compressible;
    memory = kapi->allocateMemory(device, &params);
    if (memory == NULL && p.block_linear && (p.flags & NV_GEM_ALLOC_NO_SCANOUT))
    {
        params.useVideoMemory = NV_FALSE;
        memory = kapi->allocateMemory(device, &params);
    }
    if (memory == NULL)
    {
        drm_say("allocating %llu bytes (block linear %u) failed\n",
                (unsigned long long)p.memory_size, p.block_linear);
        return -EINVAL;
    }
    gem = gem_new(memory, p.memory_size);
    if (gem == NULL)
    {
        kapi->freeMemory(device, memory);
        return -ENOMEM;
    }
    p.handle = handle_new(file, gem);
    if (p.handle == 0)
    {
        gem_settle(gem);
        return -ENOSPC;
    }
    return copy_out(arg, &p, sizeof(p));
}

static int drm_import_nvkms(struct drm_file *file, NvU64 arg)
{
    struct drm_nvidia_gem_import_nvkms_memory_params p;
    const struct NvKmsKapiFunctionsTable *kapi = nvrm_kms_kapi();
    NvU32 gpu_id, kind;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);
    struct NvKmsKapiMemory *memory;
    struct drm_gem *gem;

    if (device == NULL)
        return -EOPNOTSUPP;
    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    /* KAPI copies the parameters from the client itself, with nvkms_copyin. */
    memory = kapi->importMemory(device, p.mem_size, p.nvkms_params_ptr, p.nvkms_params_size);
    if (memory == NULL)
        return -EINVAL;
    gem = gem_new(memory, p.mem_size);
    if (gem == NULL)
    {
        kapi->freeMemory(device, memory);
        return -ENOMEM;
    }
    p.handle = handle_new(file, gem);
    if (p.handle == 0)
    {
        gem_settle(gem);
        return -ENOSPC;
    }
    return copy_out(arg, &p, sizeof(p));
}

/*
 * A fence context: the client's semaphore surface, imported through KAPI
 * with the client's own parameters, as nvidia-drm. NVIDIA's Vulkan driver
 * makes one in vkCreateDevice once GET_DEV_INFO says semsurf, and fails
 * device creation without it. Fences on it are the N3b sync design's
 * (NVIDIA.md 4.6), not built yet.
 */
static int drm_semsurf_ctx_create(struct drm_file *file, NvU64 arg)
{
    struct drm_nvidia_semsurf_fence_ctx_create_params p;
    const struct NvKmsKapiFunctionsTable *kapi = nvrm_kms_kapi();
    NvU32 gpu_id, kind;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);
    struct NvKmsKapiSemaphoreSurface *semsurf;
    void *map = NULL, *max_submitted = NULL;
    struct drm_gem *gem;

    if (device == NULL)
        return -EOPNOTSUPP;
    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    if (p.pad != 0)
        return -EINVAL;
    semsurf = kapi->importSemaphoreSurface(device, p.nvkms_params_ptr, p.nvkms_params_size,
                                           &map, &max_submitted);
    if (semsurf == NULL)
    {
        drm_say("importing a semaphore surface failed\n");
        return -ENOMEM;
    }
    gem = gem_new(NULL, 0);
    if (gem == NULL)
    {
        kapi->freeSemaphoreSurface(device, semsurf);
        return -ENOMEM;
    }
    gem->semsurf = semsurf;
    gem->sem_index = p.index;
    gem->sem_map = map;
    p.handle = handle_new(file, gem);
    if (p.handle == 0)
    {
        gem_settle(gem);
        return -ENOSPC;
    }
    drm_say("fence context %u: semaphore %llu, mapped %d\n", p.handle,
            (unsigned long long)p.index, map != NULL);
    return copy_out(arg, &p, sizeof(p));
}

/*
 * Fences (N3b sync, NVIDIA.md 4.6; consultant ledger 647). Each is a kernel
 * sync_file named by a cookie that is never reused, signalled from KAPI's
 * semaphore-surface callback once the semaphore reaches the wait value. The
 * kernel signals it ETIMEDOUT at its deadline and ENODEV if nvrm dies, so a
 * fence nvrm never signals still ends; a late signal is refused, harmlessly.
 */
struct drm_fence {
    NvU64 cookie;
    struct drm_gem *ctx;
};

static NvU64 next_fence_cookie = 1;
extern int nvrm_trace_drm;

/* KAPI's callback: the semaphore reached the value. */
static void drm_fence_reached(void *data)
{
    struct drm_fence *fence = data;
    int rc = nvos_chardev_sync_signal(fence->cookie, 0);

    if (nvrm_trace_drm)
        drm_say("fence %llu reached, signal %d\n", (unsigned long long)fence->cookie, rc);
    /*
     * Called from inside RM, which holds its own locks: gems_lock is taken
     * for the count alone, and no holder of it calls RM (see gems_lock).
     * Not settled here: freeing the semaphore surface from inside its own
     * callback is not safe. A context closed before its last fence keeps
     * its surface until nvrm exits.
     */
    nvos_mutex_lock(&gems_lock);
    fence->ctx->fences--;
    nvos_mutex_unlock(&gems_lock);
    free(fence);
}

/*
 * The fence context `handle` names in `file`, counted as holding one more
 * fence, or NULL. Looked up and counted under the file's lock, which a
 * GEM_CLOSE of the handle takes first, so the context cannot be ended
 * between the two.
 */
static struct drm_gem *fence_context_hold(struct drm_file *file, NvU32 handle)
{
    struct drm_gem *ctx = NULL;

    nvos_mutex_lock(&file->lock);
    if (handle != 0 && handle <= file->room)
        ctx = file->table[handle - 1];
    if (ctx != NULL && ctx->semsurf == NULL)
        ctx = NULL;
    if (ctx != NULL)
    {
        nvos_mutex_lock(&gems_lock);
        ctx->fences++;
        nvos_mutex_unlock(&gems_lock);
    }
    nvos_mutex_unlock(&file->lock);
    return ctx;
}

/* A fence that never reached KAPI's callback is over: its context has one
 * fewer, and ends if that was all that kept it. */
static void fence_context_drop(struct drm_gem *ctx)
{
    NvBool dead;

    nvos_mutex_lock(&gems_lock);
    ctx->fences--;
    dead = gem_unlink_locked(ctx);
    nvos_mutex_unlock(&gems_lock);
    if (dead)
        gem_free(ctx);
}

static int drm_semsurf_fence_create(struct drm_file *file, NvU64 request, NvU64 arg)
{
    struct drm_nvidia_semsurf_fence_create_params p;
    const struct NvKmsKapiFunctionsTable *kapi = nvrm_kms_kapi();
    NvU32 gpu_id, kind;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);
    struct NvKmsKapiSemaphoreSurfaceCallback *handle = NULL;
    struct drm_fence *fence;
    struct drm_gem *ctx;
    NvKmsKapiRegisterWaiterResult result;
    NvU64 cookie, sem_index;
    NvS64 fd;

    if (device == NULL)
        return -EOPNOTSUPP;
    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    if (p.pad != 0)
        return -EINVAL;
    fence = calloc(1, sizeof(*fence));
    if (fence == NULL)
        return -ENOMEM;
    ctx = fence_context_hold(file, p.fence_context_handle);
    if (ctx == NULL)
    {
        free(fence);
        return -EINVAL;
    }
    nvos_mutex_lock(&gems_lock);
    cookie = next_fence_cookie++;
    nvos_mutex_unlock(&gems_lock);
    fence->cookie = cookie;
    fence->ctx = ctx;
    sem_index = ctx->sem_index;
    fd = nvos_chardev_sync_install(request, cookie, p.timeout_value_ms, NVOS_SYNC_CLOEXEC);
    if (fd < 0)
    {
        fence_context_drop(ctx);
        free(fence);
        return (int)fd;
    }
    p.fd = (NvS32)fd;
    /*
     * Once registered, the fence is the callback's: it may be reached, and
     * freed, and its context closed, before the next line here runs. So
     * what is said below was copied out first.
     */
    result = kapi->registerSemaphoreSurfaceCallback(device, ctx->semsurf, drm_fence_reached, fence,
                                                    sem_index, p.wait_value, 0, &handle);
    if (nvrm_trace_drm)
        drm_say("fence %llu: semaphore %llu wait %llu, fd %d, waiter %d\n",
                (unsigned long long)cookie, (unsigned long long)sem_index,
                (unsigned long long)p.wait_value, p.fd, (int)result);
    if (result != NVKMS_KAPI_REG_WAITER_SUCCESS)
    {
        /* Reached already: signalled now. Not registered: an error. */
        (void)nvos_chardev_sync_signal(cookie,
                                       result == NVKMS_KAPI_REG_WAITER_ALREADY_SIGNALLED ? 0 : -EIO);
        /* No callback will come for it: the context is let go here, and
         * ends here if it was closed meanwhile. */
        fence_context_drop(ctx);
        free(fence);
    }
    return copy_out(arg, &p, sizeof(p));
}

static int drm_export_nvkms(struct drm_file *file, NvU64 arg)
{
    struct drm_nvidia_gem_export_nvkms_memory_params p;
    NvU32 gpu_id, kind;
    struct NvKmsKapiDevice *device = nvrm_kms_device(&gpu_id, &kind);
    struct drm_gem *gem;

    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    gem = handle_gem(file, p.handle);
    if (gem == NULL || device == NULL || gem->memory == NULL)
        return -EINVAL;
    return nvrm_kms_kapi()->exportMemory(device, gem->memory, p.nvkms_params_ptr,
                                         p.nvkms_params_size)
               ? 0
               : -EINVAL;
}

static int drm_map_offset(struct drm_file *file, NvU64 arg)
{
    struct drm_nvidia_gem_map_offset_params p;
    struct drm_gem *gem;

    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    gem = handle_gem(file, p.handle);
    if (gem == NULL || gem->memory == NULL)
        return -EINVAL;
    /* A fake offset naming the object: its cookie, in pages. */
    p.offset = gem->cookie << PAGE_SHIFT;
    return copy_out(arg, &p, sizeof(p));
}

static int drm_identify(struct drm_file *file, NvU64 arg)
{
    struct drm_nvidia_gem_identify_object_params p;

    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    if (handle_gem(file, p.handle) == NULL)
        return -EINVAL;
    p.object_type = 0;      /* every object here is NVKMS memory */
    return copy_out(arg, &p, sizeof(p));
}

static int drm_gem_close(struct drm_file *file, NvU64 arg)
{
    struct drm_gem_close p;

    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    return handle_close(file, p.handle);
}

/* A name-only dmabuf's size: the object's, in whole pages. */
static NvU64 gem_name_size(const struct drm_gem *gem)
{
    return (gem->size + PAGE_SIZE - 1) & ~(NvU64)(PAGE_SIZE - 1);
}

static int drm_handle_to_fd(struct drm_file *file, NvU64 request, NvU64 arg)
{
    struct drm_prime_handle p;
    struct drm_gem *gem;
    NvU32 vmo = 0, flags = 0;
    NvS64 fd;
    NvBool made;

    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    gem = handle_gem(file, p.handle);
    if (gem == NULL)
        return -ENOENT;
    if (p.flags & DRM_RDWR)
        flags |= NVOS_DMABUF_WRITABLE;
    if (p.flags & DRM_CLOEXEC)
        flags |= NVOS_DMABUF_CLOEXEC;
    flags |= NVOS_DMABUF_TELL_MADE;
    /*
     * System memory is a dmabuf a compositor can map: its VMO. Anything
     * else lives in video memory and is a name-only dmabuf, which only
     * NVIDIA's userspace imports again, through PRIME_FD_TO_HANDLE here.
     */
    if (gem->at != NULL && gem->at->pages != NULL && nvos_pages_vmo(gem->at->pages, &vmo) == NV_OK)
        fd = nvos_chardev_dmabuf_install(request, vmo, gem->cookie, flags);
    else
        fd = nvos_chardev_dmabuf_install_name(request, gem_name_size(gem), gem->cookie, flags);
    if (fd < 0)
        return (int)fd;
    made = (fd & NVOS_DMABUF_MADE) != 0;
    fd &= ~NVOS_DMABUF_MADE;
    if (made)
    {
        nvos_mutex_lock(&gems_lock);
        gem->dmabufs++;
        nvos_mutex_unlock(&gems_lock);
    }
    p.fd = (NvS32)fd;
    return copy_out(arg, &p, sizeof(p));
}

static int drm_fd_to_handle(struct drm_file *file, NvU64 request, NvU64 arg)
{
    struct drm_prime_handle p;
    struct drm_gem *gem;
    NvU64 cookie = 0;
    int rc;

    if (copy_in(&p, arg, sizeof(p)) != 0)
        return -EFAULT;
    rc = nvos_chardev_dmabuf_resolve(request, p.fd, &cookie);
    if (rc != 0)
        return rc == -EBADF ? -EBADF : -EINVAL;
    nvos_mutex_lock(&gems_lock);
    gem = gem_by_cookie_locked(cookie);
    nvos_mutex_unlock(&gems_lock);
    if (gem == NULL)
        return -EINVAL;
    /* As Linux: the same object, under the handle this file has for it. */
    p.handle = handle_of(file, gem);
    if (p.handle == 0)
        p.handle = handle_new(file, gem);
    if (p.handle == 0)
        return -ENOSPC;
    return copy_out(arg, &p, sizeof(p));
}

static int drm_ioctl_one(void *opened, NvU64 request, NvU32 cmd, NvU64 arg);

/* Bring-up aid (N3b): say every ioctl and its answer. */
int nvrm_trace_drm = 1;

int nvrm_drm_ioctl(void *opened, NvU64 request, NvU32 cmd, NvU64 arg)
{
    int rc = drm_ioctl_one(opened, request, cmd, arg);

    if (nvrm_trace_drm)
        drm_say("ioctl 0x%x = %d\n", cmd, rc);
    return rc;
}

static int drm_ioctl_one(void *opened, NvU64 request, NvU32 cmd, NvU64 arg)
{
    struct drm_file *file = opened;

    switch (cmd)
    {
    case DRM_IOCTL_VERSION:
        return drm_version(arg);
    case DRM_IOCTL_GET_CAP:
        return drm_get_cap(arg);
    case DRM_IOCTL_SET_CLIENT_CAP:
        /* Atomic and universal planes are a primary node's. */
        return -EINVAL;
    case DRM_IOCTL_GEM_CLOSE:
        return drm_gem_close(file, arg);
    case DRM_IOCTL_PRIME_HANDLE_TO_FD:
        return drm_handle_to_fd(file, request, arg);
    case DRM_IOCTL_PRIME_FD_TO_HANDLE:
        return drm_fd_to_handle(file, request, arg);
    case DRM_IOCTL_NVIDIA_GET_DEV_INFO:
        return drm_dev_info(arg);
    case DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY:
        return drm_alloc(file, arg);
    case DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY:
        return drm_import_nvkms(file, arg);
    case DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY:
        return drm_export_nvkms(file, arg);
    case DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET:
        return drm_map_offset(file, arg);
    case DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT:
        return drm_identify(file, arg);
    case DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED:
        return 0;
    case DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CTX_CREATE:
        return drm_semsurf_ctx_create(file, arg);
    case DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE:
        return drm_semsurf_fence_create(file, request, arg);
    case DRM_IOCTL_NVIDIA_GET_DRM_FILE_UNIQUE_ID:
        return copy_out(arg, &file->unique, sizeof(file->unique));
    default:
        drm_say("ioctl 0x%x not offered\n", cmd);
        return -EINVAL;
    }
}

/* An mmap at GEM_MAP_OFFSET's fake offset: the object's VMO, whole. */
void nvrm_drm_mmap(void *opened, const struct nvos_request *request)
{
    struct drm_gem *gem;
    NvU32 vmo = 0;

    (void)opened;
    nvos_mutex_lock(&gems_lock);
    gem = gem_by_cookie_locked(request->arg >> PAGE_SHIFT);
    nvos_mutex_unlock(&gems_lock);
    if (gem == NULL || (request->arg & (PAGE_SIZE - 1)) != 0 || gem->at == NULL ||
        gem->at->pages == NULL || (NvU64)request->pages > gem->at->num_pages ||
        nvos_pages_vmo(gem->at->pages, &vmo) != NV_OK)
    {
        (void)nvos_chardev_reply(request->id, -EINVAL, 0);
        return;
    }
    (void)nvos_chardev_reply_map(request->id, vmo, 0, NVOS_MAP_VMO);
}
