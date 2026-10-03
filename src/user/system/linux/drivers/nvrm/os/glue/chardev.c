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
    struct chardev_file *next;
};

static struct chardev_file *files;
static nvos_mutex_t files_lock;

static void remember(NvU64 file, nv_linux_file_private_t *nvlfp, struct chardev_file *entry)
{
    entry->file = file;
    entry->nvlfp = nvlfp;
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
    return nvos_chardev_copy_in(*(NvU64 *)context, to, from, length) == 0 ? 0 : -EFAULT;
}

static int bridge_out(void *context, NvU64 to, const void *from, NvU32 length)
{
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
    rc = nvrm_ioctl(work->nvlfp, work->request.cmd, (void *)(NvUPtr)work->request.arg);
    nvos_client_leave();
    (void)nvos_chardev_reply(work->request.id, rc < 0 ? rc : 0, rc > 0 ? rc : 0);
    free(work);
}

static void handle(const struct nvos_request *request)
{
    nv_linux_file_private_t *nvlfp;
    struct chardev_file *entry;
    struct worker *work;

    switch (request->op)
    {
        case NVOS_REQUEST_OPEN:
            entry = calloc(1, sizeof(*entry));
            if (entry == NULL)
            {
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
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
            remember(request->file, nvlfp, entry);
            (void)nvos_chardev_reply(request->id, 0, 0);
            return;

        case NVOS_REQUEST_IOCTL:
            nvlfp = find(request->file);
            if (nvlfp == NULL)
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
            if (!nvos_thread_spawn(serve_ioctl, work))
            {
                free(work);
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
            }
            return;

        case NVOS_REQUEST_RELEASE:
            entry = forget(request->file);
            if (entry != NULL)
            {
                nvrm_close(entry->nvlfp);
                free(entry);
            }
            return;

        default:
            return;
    }
}

int nvrm_chardev_serve(const NvU16 *minors, NvU32 count)
{
    struct nvos_request request;

    if (nvos_chardev_start(minors, count) != NV_OK)
        return -1;
    for (;;)
    {
        if (nvos_chardev_next(&request) != NV_OK)
            return -2;
        handle(&request);
    }
}
