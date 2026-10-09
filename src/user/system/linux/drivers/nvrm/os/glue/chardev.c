/*
 * nvrm's device files: the dispatcher for the requests the kernel's chardev
 * core forwards (docs/NVIDIA.md §4.4, N1e).
 *
 * nvrm says HELLO with /dev/nvidiactl and one /dev/nvidia<N> per GPU it
 * started, then takes REQUESTs one by one:
 *
 *  - OPEN opens the control device or a GPU (os/kept/nv.c) and remembers
 *    the file by the identity the kernel gave it;
 *  - IOCTL runs on a thread of its own, since RM may sleep, as the client
 *    the request names: its copy_in and copy_out are the bridge's, so RM's
 *    os_memcpy_*_user reach the program waiting in the call;
 *  - MMAP maps what NV_ESC_RM_MAP_MEMORY left on the file, as NVIDIA's
 *    nvidia_mmap_helper decides it: a GPU file's range of the card's BARs,
 *    uncached for registers and USERD and as RM asked for the framebuffer;
 *    the control file's system memory, the VMO the allocation is. The
 *    kernel checks the answer against the device and the VMO's rights;
 *  - RELEASE closes the file. The kernel sends one for a file it may never
 *    have heard back about, so an unknown file is no error.
 *
 * Each OPEN and IOCTL is answered with nvos_chardev_reply.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"

#ifndef EBADF
#define EBADF   9
#endif
#ifndef ENODEV
#define ENODEV  19
#endif

/* An open file, by the kernel's identity for it. */
struct chardev_file {
    NvU64 file;
    nv_linux_file_private_t *nvlfp;
    /* An open of the render node (os/glue/drm.c) in place of nvlfp. */
    void *drm;
    struct chardev_file *next;
};

static struct chardev_file *files;
static nvos_mutex_t files_lock;

/* The request rate and the bytes copied for the clients, said every five
 * seconds while requests come: what decides whether the path wants shared
 * memory (N3c measurement). */
static NvU64 count_ioctl, count_mmap, count_other, bytes_in, bytes_out;
static struct nvos_timer *count_timer;

#define COUNT_SPAN 5

static void count_say(void *unused)
{
    NvU64 ioctl, mmap, other, in, out;

    (void)unused;
    ioctl = __atomic_exchange_n(&count_ioctl, 0, __ATOMIC_RELAXED);
    mmap = __atomic_exchange_n(&count_mmap, 0, __ATOMIC_RELAXED);
    other = __atomic_exchange_n(&count_other, 0, __ATOMIC_RELAXED);
    in = __atomic_exchange_n(&bytes_in, 0, __ATOMIC_RELAXED);
    out = __atomic_exchange_n(&bytes_out, 0, __ATOMIC_RELAXED);
    if (ioctl + mmap + other != 0)
        nv_printf(NV_DBG_ERRORS, "nvrm: chardev: %llu req/s (ioctl %llu, mmap %llu), copy-in %llu KB/s, copy-out %llu KB/s\n",
                  (ioctl + mmap + other) / COUNT_SPAN, ioctl / COUNT_SPAN, mmap / COUNT_SPAN,
                  in / COUNT_SPAN / 1024, out / COUNT_SPAN / 1024);
    nvos_timer_start(count_timer, (NvU64)COUNT_SPAN * 1000000000ULL);
}

static void remember(NvU64 file, nv_linux_file_private_t *nvlfp, void *drm,
                     struct chardev_file *entry)
{
    entry->file = file;
    entry->nvlfp = nvlfp;
    entry->drm = drm;
    nvos_mutex_lock(&files_lock);
    entry->next = files;
    files = entry;
    nvos_mutex_unlock(&files_lock);
}

static nv_linux_file_private_t *find(NvU64 file)
{
    struct chardev_file *entry;
    nv_linux_file_private_t *found = NULL;

    nvos_mutex_lock(&files_lock);
    for (entry = files; entry != NULL; entry = entry->next)
        if (entry->file == file)
        {
            found = entry->nvlfp;
            break;
        }
    nvos_mutex_unlock(&files_lock);
    return found;
}

/* The render node's open `file` is, or NULL for another file. */
static void *find_drm(NvU64 file)
{
    struct chardev_file *entry;
    void *found = NULL;

    nvos_mutex_lock(&files_lock);
    for (entry = files; entry != NULL; entry = entry->next)
        if (entry->file == file)
        {
            found = entry->drm;
            break;
        }
    nvos_mutex_unlock(&files_lock);
    return found;
}

static struct chardev_file *forget(NvU64 file)
{
    struct chardev_file **at, *entry = NULL;

    nvos_mutex_lock(&files_lock);
    for (at = &files; *at != NULL; at = &(*at)->next)
        if ((*at)->file == file)
        {
            entry = *at;
            *at = entry->next;
            break;
        }
    nvos_mutex_unlock(&files_lock);
    return entry;
}

/* The client's copies, through the bridge, for the request in context. */
static int bridge_in(void *context, void *to, NvU64 from, NvU32 length)
{
    __atomic_fetch_add(&bytes_in, length, __ATOMIC_RELAXED);
    return nvos_chardev_copy_in(*(NvU64 *)context, to, from, length) == 0 ? 0 : -EFAULT;
}

static int bridge_out(void *context, NvU64 to, const void *from, NvU32 length)
{
    __atomic_fetch_add(&bytes_out, length, __ATOMIC_RELAXED);
    return nvos_chardev_copy_out(*(NvU64 *)context, to, from, length) == 0 ? 0 : -EFAULT;
}

static NvS64 bridge_fd(void *context, int fd)
{
    return nvos_chardev_file(*(NvU64 *)context, fd);
}

/* One ioctl's worker. */
struct worker {
    struct nvos_request request;
    nv_linux_file_private_t *nvlfp;
    void *drm;
};

static void serve_ioctl(void *argument)
{
    struct worker *work = argument;
    nvos_client_t client;
    int rc;

    os_mem_set(&client, 0, sizeof(client));
    client.pid = work->request.pid;
    client.euid = work->request.euid;
    client.administrator = (work->request.euid == 0);
    client.copy_in = bridge_in;
    client.copy_out = bridge_out;
    client.context = &work->request.id;
    client.resolve_fd = bridge_fd;

    if (!nvos_client_enter(&client))
    {
        (void)nvos_chardev_reply(work->request.id, -EBUSY, 0);
        free(work);
        return;
    }
    if (work->drm != NULL)
        rc = nvrm_drm_ioctl(work->drm, work->request.id, work->request.cmd, work->request.arg);
    else
        rc = nvrm_ioctl(work->nvlfp, work->request.cmd, (void *)(NvUPtr)work->request.arg);
    nvos_client_leave();
    (void)nvos_chardev_reply(work->request.id, rc < 0 ? rc : 0, rc > 0 ? rc : 0);
    free(work);
}

#ifndef ENXIO
#define ENXIO   6
#endif
#ifndef ERANGE
#define ERANGE  34
#endif

/* An MMAP: what the file's mapping context names, as nvidia_mmap_helper
 * reads it. */
static void serve_mmap(const struct nvos_request *request)
{
    nv_linux_file_private_t *nvlfp = find(request->file);
    const nv_alloc_mapping_context_t *context;
    nv_state_t *nv;
    NvU64 length = (NvU64)request->pages << PAGE_SHIFT;

    if (nvlfp == NULL)
    {
        (void)nvos_chardev_reply(request->id, -EBADF, 0);
        return;
    }
    context = &nvlfp->mmap_context;
    nv = NV_STATE_PTR(nvlfp->nvptr);
    if (!context->valid || request->arg != 0)
    {
        nvrm_say("mmap: no valid mapping context on the file\n");
        (void)nvos_chardev_reply(request->id, -EINVAL, 0);
        return;
    }
    if (!NV_IS_CTL_DEVICE(nv))
    {
        NvU64 kind = NVOS_MAP_APERTURE;

        if (context->memArea.numRanges != 1 ||
            context->memArea.pRanges[0].size != length)
        {
            nvrm_say("mmap: %llu ranges, or not %llu bytes\n",
                     (unsigned long long)context->memArea.numRanges,
                     (unsigned long long)length);
            (void)nvos_chardev_reply(request->id, -ENXIO, 0);
            return;
        }
        if (IS_FB_OFFSET(nv, context->access_start, context->access_size) &&
            !IS_UD_OFFSET(nv, context->access_start, context->access_size) &&
            context->caching == NV_MEMORY_WRITECOMBINED)
            kind |= NVOS_MAP_WRITE_COMBINING;
        (void)nvos_chardev_reply_map(request->id, context->memArea.pRanges[0].start, 0, kind);
        return;
    }
    else
    {
        nv_alloc_t *at = context->alloc;
        NvU32 vmo = 0;

        if (at == NULL || context->page_index + request->pages > at->num_pages)
        {
            (void)nvos_chardev_reply(request->id, -ERANGE, 0);
            return;
        }
        if (nvos_pages_vmo(at->pages, &vmo) != NV_OK)
        {
            (void)nvos_chardev_reply(request->id, -ENXIO, 0);
            return;
        }
        (void)nvos_chardev_reply_map(request->id, vmo, context->page_index << PAGE_SHIFT,
                                     NVOS_MAP_VMO);
    }
}

static void handle(const struct nvos_request *request)
{
    nv_linux_file_private_t *nvlfp;
    struct chardev_file *entry;
    struct worker *work;
    void *drm;

    if (request->op == NVOS_REQUEST_IOCTL)
        __atomic_fetch_add(&count_ioctl, 1, __ATOMIC_RELAXED);
    else if (request->op == NVOS_REQUEST_MMAP)
        __atomic_fetch_add(&count_mmap, 1, __ATOMIC_RELAXED);
    else
        __atomic_fetch_add(&count_other, 1, __ATOMIC_RELAXED);

    switch (request->op)
    {
        case NVOS_REQUEST_OPEN:
            entry = calloc(1, sizeof(*entry));
            if (entry == NULL)
            {
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
                return;
            }
            if (request->minor == NVRM_RENDER_MINOR)
            {
                drm = nvrm_drm_open();
                if (drm == NULL)
                {
                    free(entry);
                    (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
                    return;
                }
                remember(request->file, NULL, drm, entry);
                (void)nvos_chardev_reply(request->id, 0, 0);
                return;
            }
            nvlfp = (request->minor == 255) ? nvrm_open_ctl() : nvrm_open_gpu(request->minor);
            if (nvlfp == NULL)
            {
                free(entry);
                (void)nvos_chardev_reply(request->id, -ENODEV, 0);
                return;
            }
            /* RM names files by fd (nv_get_file_private): the kernel's
             * identity for it, which chardev_file resolves descriptors to. */
            nvlfp->fd = (NvS32)request->file;
            remember(request->file, nvlfp, NULL, entry);
            (void)nvos_chardev_reply(request->id, 0, 0);
            return;

        case NVOS_REQUEST_IOCTL:
            nvlfp = find(request->file);
            drm = find_drm(request->file);
            if (nvlfp == NULL && drm == NULL)
            {
                (void)nvos_chardev_reply(request->id, -EBADF, 0);
                return;
            }
            work = calloc(1, sizeof(*work));
            if (work == NULL)
            {
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
                return;
            }
            work->request = *request;
            work->nvlfp = nvlfp;
            work->drm = drm;
            if (!nvos_thread_spawn(serve_ioctl, work))
            {
                free(work);
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
            }
            return;

        case NVOS_REQUEST_MMAP:
            drm = find_drm(request->file);
            if (drm != NULL)
                nvrm_drm_mmap(drm, request);
            else
                serve_mmap(request);
            return;

        case NVOS_REQUEST_RELEASE:
            entry = forget(request->file);
            if (entry != NULL)
            {
                if (entry->drm != NULL)
                    nvrm_drm_close(entry->drm);
                else
                    nvrm_close(entry->nvlfp);
                free(entry);
            }
            return;

        case NVOS_REQUEST_DMABUF_RELEASE:
            nvrm_drm_released(request->arg);
            return;

        default:
            return;
    }
}

int nvrm_chardev_publish(const NvU16 *minors, NvU32 count)
{
    return nvos_chardev_start(minors, count) == NV_OK ? 0 : -1;
}

int nvrm_chardev_serve(void)
{
    struct nvos_request request;

    count_timer = nvos_timer_create(count_say, NULL);
    if (count_timer != NULL)
        nvos_timer_start(count_timer, (NvU64)COUNT_SPAN * 1000000000ULL);
    for (;;)
    {
        if (nvos_chardev_next(&request) != NV_OK)
            return -2;
        handle(&request);
    }
}
