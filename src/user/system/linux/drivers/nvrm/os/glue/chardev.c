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

/* ------------------------------------------------------------------------
 * The stall diagnostic (2026-10-10, the desktop stall of nv-next on the
 * RTX 3060): which requests are in nvrm and not answered. On that boot two
 * ioctl threads never finished and nothing said which calls they were.
 *
 * Every ioctl is kept in a table from the moment it is taken until it is
 * answered: its id, the program's pid, the node, the command, and for an RM
 * call what os/kept/nv.c read of it (nvrm_diag_rm: the escape and its first
 * four words, which are a control's client, object and command and an
 * allocation's parent, handle and class). A thread of its own, which takes
 * none of RM's locks and no lock of the glue but the table's, says every
 * five seconds each request that has been in for five or more, and says so
 * too when the dispatch thread itself has been inside one request that
 * long: nothing is taken from the kernel meanwhile. A request said once is
 * said again when it is answered after all.
 *
 * One switch, nvrm_diag, which os/glue/drm.c reads too. On in this branch;
 * to be 0 before it lands.
 * ---------------------------------------------------------------------- */
int nvrm_diag = 1;

#define DIAG_SLOTS      128
#define DIAG_OLD_NS     (5ULL * 1000000000ULL)
#define DIAG_LOOK_MS    1000
/* The most requests said in one look; the rest are counted. */
#define DIAG_SAID       16

struct diag_request {
    NvBool used;
    NvBool drm;
    NvBool rm_known;
    NvU64 id;
    NvU64 began;
    NvU64 said;         /* when it was last said, 0 if never */
    NvU64 thread;       /* the worker serving it, 0 before it runs */
    NvU32 pid;
    NvU32 minor;
    NvU32 cmd;
    NvU32 escape;
    NvU32 words[4];
};

static struct diag_request diag_table[DIAG_SLOTS];
static nvos_mutex_t diag_lock;
/* Requests that found the table full, so are not watched. */
static NvU64 diag_unwatched;

/* What the dispatch thread is inside, 0 in `began` when it waits for the
 * next request. Written by that thread alone, read by the watcher. */
static NvU64 diag_dispatch_began;
static NvU32 diag_dispatch_op, diag_dispatch_pid, diag_dispatch_minor;
static NvU64 diag_dispatch_id;

/*
 * A line of the diagnostic's own. Not through nv_printf: that takes the
 * print lock, and the watcher must be able to speak whatever another thread
 * holds. One write a line, so it is not cut by another thread's.
 */
__attribute__((format(printf, 1, 2))) static void diag_line(const char *format, ...)
{
    char line[320];
    va_list arguments;
    int used = snprintf(line, sizeof(line), "nvrm: diag: ");

    va_start(arguments, format);
    (void)vsnprintf(line + used, sizeof(line) - (size_t)used - 1, format, arguments);
    va_end(arguments);
    used = (int)strlen(line);
    line[used] = '\n';
    line[used + 1] = '\0';
    nvos_write_line(line);
}

static const char *diag_op_name(NvU32 op)
{
    switch (op)
    {
        case NVOS_REQUEST_OPEN:           return "OPEN";
        case NVOS_REQUEST_IOCTL:          return "IOCTL (starting its thread)";
        case NVOS_REQUEST_RELEASE:        return "RELEASE";
        case NVOS_REQUEST_MMAP:           return "MMAP";
        case NVOS_REQUEST_DMABUF_RELEASE: return "DMABUF_RELEASE";
        default:                          return "an unknown request";
    }
}

/* Keep an ioctl just taken; its slot, or -1. */
static int diag_enter(const struct nvos_request *request, NvBool drm)
{
    int slot = -1, i;

    if (!nvrm_diag)
        return -1;
    nvos_mutex_lock(&diag_lock);
    for (i = 0; i < DIAG_SLOTS; i++)
    {
        if (!diag_table[i].used)
        {
            os_mem_set(&diag_table[i], 0, sizeof(diag_table[i]));
            diag_table[i].used = NV_TRUE;
            diag_table[i].drm = drm;
            diag_table[i].id = request->id;
            diag_table[i].began = os_get_monotonic_time_ns();
            diag_table[i].pid = request->pid;
            diag_table[i].minor = request->minor;
            diag_table[i].cmd = request->cmd;
            slot = i;
            break;
        }
    }
    if (slot < 0)
        diag_unwatched++;
    nvos_mutex_unlock(&diag_lock);
    return slot;
}

/* The calling thread serves the request in `slot`. */
static void diag_serving(int slot)
{
    NvU64 thread = 0;

    if (slot < 0)
        return;
    (void)os_get_current_thread(&thread);
    nvos_mutex_lock(&diag_lock);
    diag_table[slot].thread = thread;
    nvos_mutex_unlock(&diag_lock);
}

/* The request in `slot` is answered, or was never started. */
static void diag_leave(int slot)
{
    struct diag_request was;

    if (slot < 0)
        return;
    nvos_mutex_lock(&diag_lock);
    was = diag_table[slot];
    diag_table[slot].used = NV_FALSE;
    nvos_mutex_unlock(&diag_lock);
    if (was.said != 0)
        diag_line("request %llu (pid %u, ioctl 0x%x) is answered after %llu ms",
                 (unsigned long long)was.id, was.pid, was.cmd,
                 (unsigned long long)((os_get_monotonic_time_ns() - was.began) / 1000000ULL));
}

/*
 * From os/kept/nv.c, once an RM ioctl's argument is read: which call the
 * calling thread's request is.
 */
void nvrm_diag_rm(NvU32 escape, const void *argument, NvU32 size)
{
    const NvU32 *words = argument;
    NvU64 thread = 0;
    int i, w;

    if (!nvrm_diag)
        return;
    (void)os_get_current_thread(&thread);
    nvos_mutex_lock(&diag_lock);
    for (i = 0; i < DIAG_SLOTS; i++)
    {
        if (diag_table[i].used && diag_table[i].thread == thread && thread != 0)
        {
            diag_table[i].rm_known = NV_TRUE;
            diag_table[i].escape = escape;
            for (w = 0; w < 4; w++)
                diag_table[i].words[w] =
                    (words != NULL && size >= (NvU32)(w + 1) * sizeof(NvU32)) ? words[w] : 0;
            break;
        }
    }
    nvos_mutex_unlock(&diag_lock);
}

static void diag_say(const struct diag_request *request, NvU64 now)
{
    unsigned long long seconds = (unsigned long long)((now - request->began) / 1000000000ULL);
    char what[96];

    if (request->drm)
        snprintf(what, sizeof(what), "render ioctl 0x%x", request->cmd);
    else if (!request->rm_known)
        snprintf(what, sizeof(what), "%s ioctl 0x%x, argument not read yet",
                 request->minor == 255 ? "ctl" : "gpu", request->cmd);
    else if (request->escape == 0x2A)   /* NV_ESC_RM_CONTROL */
        snprintf(what, sizeof(what), "%s control 0x%x on object 0x%x of client 0x%x",
                 request->minor == 255 ? "ctl" : "gpu", request->words[2], request->words[1],
                 request->words[0]);
    else if (request->escape == 0x2B)   /* NV_ESC_RM_ALLOC */
        snprintf(what, sizeof(what), "%s alloc of class 0x%x as 0x%x under 0x%x of client 0x%x",
                 request->minor == 255 ? "ctl" : "gpu", request->words[3], request->words[2],
                 request->words[1], request->words[0]);
    else
        snprintf(what, sizeof(what), "%s escape 0x%x, words 0x%x 0x%x 0x%x 0x%x",
                 request->minor == 255 ? "ctl" : "gpu", request->escape, request->words[0],
                 request->words[1], request->words[2], request->words[3]);
    diag_line("request %llu of pid %u unanswered for %llu s on thread %llu: %s",
             (unsigned long long)request->id, request->pid, seconds,
             (unsigned long long)request->thread, what);
}

/* The watcher's thread. */
static void diag_watch(void *unused)
{
    static struct diag_request old[DIAG_SAID];
    NvU64 dispatch_said = 0, dispatch_said_for = 0;

    (void)unused;
    for (;;)
    {
        NvU64 now, began, unwatched;
        NvU32 count = 0, more = 0, in = 0;
        int i;

        (void)os_delay(DIAG_LOOK_MS);
        now = os_get_monotonic_time_ns();

        nvos_mutex_lock(&diag_lock);
        for (i = 0; i < DIAG_SLOTS; i++)
        {
            struct diag_request *request = &diag_table[i];

            if (!request->used)
                continue;
            in++;
            if (now - request->began < DIAG_OLD_NS ||
                (request->said != 0 && now - request->said < DIAG_OLD_NS))
                continue;
            if (count == DIAG_SAID)
            {
                more++;
                continue;
            }
            request->said = now;
            old[count++] = *request;
        }
        unwatched = diag_unwatched;
        nvos_mutex_unlock(&diag_lock);

        for (i = 0; i < (int)count; i++)
            diag_say(&old[i], now);
        if (count != 0)
            diag_line("%u request(s) are in nvrm%s; %llu were never watched (table full)",
                     in, more != 0 ? ", more old ones than are said at once" : "",
                     (unsigned long long)unwatched);

        began = __atomic_load_n(&diag_dispatch_began, __ATOMIC_ACQUIRE);
        if (began != 0 && now - began >= DIAG_OLD_NS &&
            (dispatch_said_for != began || now - dispatch_said >= DIAG_OLD_NS))
        {
            dispatch_said = now;
            dispatch_said_for = began;
            diag_line("the dispatch thread has been inside %s (request %llu, pid %u, minor 0x%x) for %llu s; no request is taken meanwhile",
                     diag_op_name(diag_dispatch_op), (unsigned long long)diag_dispatch_id,
                     diag_dispatch_pid, diag_dispatch_minor,
                     (unsigned long long)((now - began) / 1000000000ULL));
        }
        else if (began == 0 && dispatch_said_for != 0)
        {
            diag_line("the dispatch thread takes requests again");
            dispatch_said_for = 0;
        }
        else if (began != 0 && dispatch_said_for != 0 && dispatch_said_for != began)
        {
            diag_line("the dispatch thread left the request it was inside");
            dispatch_said_for = 0;
        }
    }
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
    /* Its place in the diagnostic's table, or -1. */
    int diag;
};

static void serve_ioctl(void *argument)
{
    struct worker *work = argument;
    nvos_client_t client;
    int rc;

    diag_serving(work->diag);

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
        diag_leave(work->diag);
        free(work);
        return;
    }
    if (work->drm != NULL)
        rc = nvrm_drm_ioctl(work->drm, work->request.id, work->request.cmd, work->request.arg);
    else
        rc = nvrm_ioctl(work->nvlfp, work->request.cmd, (void *)(NvUPtr)work->request.arg);
    nvos_client_leave();
    (void)nvos_chardev_reply(work->request.id, rc < 0 ? rc : 0, rc > 0 ? rc : 0);
    diag_leave(work->diag);
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

static void handle_one(const struct nvos_request *request);

/* One request, with the watcher told what the dispatch thread is inside. */
static void handle(const struct nvos_request *request)
{
    if (nvrm_diag)
    {
        diag_dispatch_op = request->op;
        diag_dispatch_pid = request->pid;
        diag_dispatch_minor = request->minor;
        diag_dispatch_id = request->id;
        __atomic_store_n(&diag_dispatch_began, os_get_monotonic_time_ns(), __ATOMIC_RELEASE);
    }
    handle_one(request);
    __atomic_store_n(&diag_dispatch_began, 0, __ATOMIC_RELEASE);
}

static void handle_one(const struct nvos_request *request)
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
            work->diag = diag_enter(request, drm != NULL);
            if (!nvos_thread_spawn(serve_ioctl, work))
            {
                diag_leave(work->diag);
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
    if (nvrm_diag)
    {
        if (nvos_thread_spawn(diag_watch, NULL))
            diag_line("on: requests unanswered for 5 s are said, every 5 s");
        else
            diag_line("no thread for the watcher; nothing will be said");
    }
    for (;;)
    {
        if (nvos_chardev_next(&request) != NV_OK)
            return -2;
        handle(&request);
    }
}
