/*
 * SPDX-FileCopyrightText: Copyright (c) 1999-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */

/*
 * Kept for Ferrix (docs/NVIDIA.md §4.2) from NVIDIA's
 * kernel-open/nvidia/nv.c at 580.173.02, under the MIT licence above.
 *
 * What is kept: the module's start and stop as far as they need no GPU
 * (nvidia_init_module, nv_module_init, nv_module_state_init,
 * nv_registry_keys_init), the control device's open and close, the ioctl
 * dispatcher (nvidia_ioctl) and its card list, per-file state and events,
 * the RC timer, firmware by name, and the nv_* calls RM makes of nv.c.
 * Each function says what changed. Their logic is NVIDIA's; their Linux
 * calls are replaced by ferrix-nvos's (include/nvos.h): semaphores and
 * spinlocks by its futex locks, kmalloc by the heap, struct file by
 * nvrm's own file identity, request_firmware by a read from the volume,
 * and pages by VMOs pinned into the card's domain.
 *
 * What is not kept: the PCI probe, nv_start_device and the GPU's file
 * open and close, interrupts and power management (N1d); poll, mmap and
 * the device files themselves (N1e); procfs (N1e); dma-buf (N3b).
 */

#define NV_FIRMWARE_FOR_NAME(name)  "nvidia/" NV_VERSION_STRING "/" name ".bin"
#include "nv-firmware.h"

#include "nv-ferrix.h"
#include "nv-reg.h"
#include "nv-chardev-numbers.h"

nv_linux_state_t *find_pci(NvU32 domain, NvU8 bus, NvU8 slot, NvU8 function);
NV_STATUS nv_parse_per_device_option_string(nvidia_stack_t *sp);
NvBool nv_is_uuid_in_gpu_exclusion_list(const char *uuid);

/* The control device: /dev/nvidiactl, 195:255. */
nv_linux_state_t nv_ctl_device = { { 0 } };

/* The root of RM's capabilities (glue/caps.c makes it). */
nv_cap_t *nvidia_caps_root = NULL;
nv_cap_t *nvos_caps_root_init(void);

/* The GPUs nvrm drives: none until N1d attaches one. */
static nv_linux_state_t *nv_linux_devices;
static nvos_sema_t nv_linux_devices_lock;
static NvU32 num_nv_devices;
#define LOCK_NV_LINUX_DEVICES()     nvos_sema_down(&nv_linux_devices_lock)
#define UNLOCK_NV_LINUX_DEVICES()   nvos_sema_up(&nv_linux_devices_lock)

/* NVIDIA's nv_system_pm_lock: readers are ioctls, the writer suspend. */
static nvos_rwlock_t nv_system_pm_lock;

/* Open files, by the identity RM's fd numbers name (nv_get_file_private). */
static nvos_mutex_t nv_open_files_lock;
static nv_linux_file_private_t *nv_open_files;
static NvS32 nv_next_fd = 1000;

/* NVIDIA's nv_dma_remap_peer_mmio, from the registry. */
static NvU32 nv_dma_remap_peer_mmio = NV_DMA_REMAP_PEER_MMIO_ENABLE;

/* ------------------------------------------------------------------------
 * Allocations: NVIDIA's nvos_create_alloc and nvos_free_alloc, on the heap.
 * ---------------------------------------------------------------------- */

static nv_alloc_t *nvos_create_alloc(NvU64 num_pages)
{
    nv_alloc_t *at;

    if (num_pages > 0xffffffffULL)
        return NULL;

    NV_KZALLOC(at, sizeof(nv_alloc_t));
    if (at == NULL)
        return NULL;

    at->page_table = calloc(num_pages ? num_pages : 1, sizeof(nvidia_pte_t));
    if (at->page_table == NULL)
    {
        NV_KFREE(at, sizeof(nv_alloc_t));
        return NULL;
    }

    at->num_pages = (unsigned int)num_pages;
    at->usage_count = 0;
    at->pid = os_get_current_process();

    return at;
}

static int nvos_free_alloc(nv_alloc_t *at)
{
    if (at == NULL)
        return -1;

    free(at->page_table);
    NV_KFREE(at, sizeof(nv_alloc_t));

    return 0;
}

/* ------------------------------------------------------------------------
 * Per-file state: NVIDIA's nv_alloc_file_private and nv_free_file_private;
 * the wait queue and spinlock are ferrix-nvos's, zero-initialized.
 * ---------------------------------------------------------------------- */

static void *nv_alloc_file_private(void)
{
    nv_linux_file_private_t *nvlfp;

    NV_KZALLOC(nvlfp, sizeof(nv_linux_file_private_t));
    if (!nvlfp)
        return NULL;

    return nvlfp;
}

static void nv_free_file_private(nv_linux_file_private_t *nvlfp)
{
    nvidia_event_t *nvet;

    if (nvlfp == NULL)
        return;

    for (nvet = nvlfp->event_data_head; nvet != NULL; nvet = nvlfp->event_data_head)
    {
        nvlfp->event_data_head = nvlfp->event_data_head->next;
        NV_KFREE(nvet, sizeof(nvidia_event_t));
    }

    if (nvlfp->mmap_context.valid)
    {
        if (nvlfp->mmap_context.page_array != NULL)
        {
            os_free_mem(nvlfp->mmap_context.page_array);
        }
        if (nvlfp->mmap_context.memArea.pRanges != NULL)
        {
            os_free_mem(nvlfp->mmap_context.memArea.pRanges);
        }
    }

    NV_KFREE(nvlfp, sizeof(nv_linux_file_private_t));
}

/* NVIDIA's nv-pci.c find_pci: the GPU at a PCI address, or NULL. */
nv_linux_state_t *find_pci(NvU32 domain, NvU8 bus, NvU8 slot, NvU8 function)
{
    nv_linux_state_t *nvl = NULL;

    LOCK_NV_LINUX_DEVICES();

    for (nvl = nv_linux_devices; nvl != NULL; nvl = nvl->next)
    {
        nv_state_t *nv = NV_STATE_PTR(nvl);

        if (nv->pci_info.domain == domain &&
            nv->pci_info.bus == bus &&
            nv->pci_info.slot == slot &&
            nv->pci_info.function == function)
        {
            break;
        }
    }

    UNLOCK_NV_LINUX_DEVICES();
    return nvl;
}

/*
 * Ferrix: NVIDIA's nv_linux_add_device_locked, for nv-pci.c's probe. The
 * GPU gets the next minor number and joins the list find_pci walks.
 */
void nv_linux_add_device(nv_linux_state_t *nvl)
{
    nv_linux_state_t *last;
    NvU32 minor = 0;

    LOCK_NV_LINUX_DEVICES();
    nvl->next = NULL;
    if (nv_linux_devices == NULL)
    {
        nv_linux_devices = nvl;
    }
    else
    {
        for (last = nv_linux_devices; ; last = last->next)
        {
            minor++;
            if (last->next == NULL)
                break;
        }
        last->next = nvl;
    }
    nvl->minor_num = minor;
    UNLOCK_NV_LINUX_DEVICES();
}

/* ------------------------------------------------------------------------
 * Module start and stop. NVIDIA's nvidia_init_module, nv_module_init and
 * nv_module_state_init, less what needs a GPU or Linux: procfs, the
 * capability device files, IMEX, NVLink and NVSwitch (stubbed, §4.2), PAT
 * setup (the kernel programs the PAT, N0c), the PCI driver's registration
 * and the character devices. Linux refuses to load without a GPU ("No
 * NVIDIA GPU found"); nvrm starts RM without one, which is what lets
 * nvrm-link-test run its no-GPU calls.
 * ---------------------------------------------------------------------- */

NvBool nv_lock_init_locks(nvidia_stack_t *sp, nv_state_t *nv)
{
    nv_linux_state_t *nvl;
    nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    nvos_sema_init(&nvl->ldata_lock, 1);
    nvos_sema_init(&nvl->mmap_lock, 1);

    atomic64_set(&nvl->usage_count, 0);

    if (!rm_init_event_locks(sp, nv))
        return NV_FALSE;

    return NV_TRUE;
}

void nv_lock_destroy_locks(nvidia_stack_t *sp, nv_state_t *nv)
{
    rm_destroy_event_locks(sp, nv);
}

static int nv_module_state_init(nvidia_stack_t *sp)
{
    nv_state_t *nv = NV_STATE_PTR(&nv_ctl_device);

    nv->os_state = (void *)&nv_ctl_device;

    if (!nv_lock_init_locks(sp, nv))
    {
        return -ENOMEM;
    }

    nv_linux_devices = NULL;
    nvos_sema_init(&nv_linux_devices_lock, 1);

    return 0;
}

static void nv_registry_keys_init(nvidia_stack_t *sp)
{
    NV_STATUS status;
    nv_state_t *nv = NV_STATE_PTR(&nv_ctl_device);
    NvU32 data;

    status = rm_read_registry_dword(sp, nv, NV_DMA_REMAP_PEER_MMIO, &data);
    if (status == NV_OK)
    {
        nv_dma_remap_peer_mmio = data;
    }
}

static nvidia_stack_t *nv_module_sp;

int nvrm_module_init(void)
{
    int rc;

    nvidia_caps_root = nvos_caps_root_init();
    if (nvidia_caps_root == NULL)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: failed to initialize capabilities.\n");
        return -ENOMEM;
    }

    rc = nv_kmem_cache_alloc_stack(&nv_module_sp);
    if (rc < 0)
        return -ENOMEM;

    if (!rm_init_rm(nv_module_sp))
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: rm_init_rm() failed!\n");
        nv_kmem_cache_free_stack(nv_module_sp);
        return -EIO;
    }

    rc = nv_module_state_init(nv_module_sp);
    if (rc < 0)
    {
        rm_shutdown_rm(nv_module_sp);
        nv_kmem_cache_free_stack(nv_module_sp);
        return rc;
    }

    nv_registry_keys_init(nv_module_sp);
    (void)nv_parse_per_device_option_string(nv_module_sp);

    nv_printf(NV_DBG_ERRORS, "NVRM: loading %s\n", pNVRM_ID);

    return 0;
}

void nvrm_module_exit(void)
{
    nv_lock_destroy_locks(nv_module_sp, NV_STATE_PTR(&nv_ctl_device));
    rm_shutdown_rm(nv_module_sp);
    nv_kmem_cache_free_stack(nv_module_sp);
    nv_module_sp = NULL;
}

/* Whether a probed GPU has RM's id `gpu_id`. */
static NvBool nv_gpu_id_known(NvU32 gpu_id)
{
    nv_linux_state_t *nvl;
    NvBool found = NV_FALSE;

    LOCK_NV_LINUX_DEVICES();
    for (nvl = nv_linux_devices; nvl != NULL; nvl = nvl->next)
    {
        if (NV_STATE_PTR(nvl)->gpu_id == gpu_id)
        {
            found = NV_TRUE;
            break;
        }
    }
    UNLOCK_NV_LINUX_DEVICES();
    return found;
}

/*
 * The GPU's device file, /dev/nvidia<minor>. NVIDIA's nvidia_open, less the
 * open-completion wait and nv_open_device's start: nvrm starts each GPU at
 * probe (os/kept/nv-pci.c), so an open only finds it and takes a reference.
 * NULL for a minor no GPU has.
 */
nv_linux_file_private_t *nvrm_open_gpu(NvU32 minor)
{
    nv_linux_state_t *nvl;
    nv_linux_file_private_t *nvlfp;

    LOCK_NV_LINUX_DEVICES();
    for (nvl = nv_linux_devices; nvl != NULL; nvl = nvl->next)
        if (nvl->minor_num == minor)
            break;
    UNLOCK_NV_LINUX_DEVICES();
    if (nvl == NULL || !(NV_STATE_PTR(nvl)->flags & NV_FLAG_OPEN))
        return NULL;

    nvlfp = nv_alloc_file_private();
    if (nvlfp == NULL)
        return NULL;
    if (nv_kmem_cache_alloc_stack(&nvlfp->sp) != 0)
    {
        nv_free_file_private(nvlfp);
        return NULL;
    }

    nvos_sema_down(&nvl->ldata_lock);
    nvlfp->nvptr = nvl;
    atomic64_inc(&nvl->usage_count);
    nvos_sema_up(&nvl->ldata_lock);

    nvlfp->open_rc = 0;
    nvlfp->adapter_status = NV_OK;

    nvos_mutex_lock(&nv_open_files_lock);
    nvlfp->fd = nv_next_fd++;
    nvlfp->next_open = nv_open_files;
    nv_open_files = nvlfp;
    nvos_mutex_unlock(&nv_open_files_lock);

    return nvlfp;
}

/* ------------------------------------------------------------------------
 * The control device. NVIDIA's nvidia_ctl_open and nvidia_ctl_close, with
 * the struct file replaced by the file identity nvrm hands out.
 * ---------------------------------------------------------------------- */

nv_linux_file_private_t *nvrm_open_ctl(void)
{
    nv_linux_state_t *nvl = &nv_ctl_device;
    nv_state_t *nv = NV_STATE_PTR(nvl);
    nv_linux_file_private_t *nvlfp;

    nv_printf(NV_DBG_INFO, "NVRM: nvidia_ctl_open\n");

    nvlfp = nv_alloc_file_private();
    if (nvlfp == NULL)
        return NULL;

    if (nv_kmem_cache_alloc_stack(&nvlfp->sp) != 0)
    {
        nv_free_file_private(nvlfp);
        return NULL;
    }

    nvos_sema_down(&nvl->ldata_lock);

    /* save the nv away in file->private_data */
    nvlfp->nvptr = nvl;

    if (atomic64_read(&nvl->usage_count) == 0)
    {
        nv->flags |= (NV_FLAG_OPEN | NV_FLAG_CONTROL);
    }

    atomic64_inc(&nvl->usage_count);
    nvos_sema_up(&nvl->ldata_lock);

    /* The control device's open never waits: it is complete at once. */
    nvlfp->open_rc = 0;
    nvlfp->adapter_status = NV_OK;

    nvos_mutex_lock(&nv_open_files_lock);
    nvlfp->fd = nv_next_fd++;
    nvlfp->next_open = nv_open_files;
    nv_open_files = nvlfp;
    nvos_mutex_unlock(&nv_open_files_lock);

    return nvlfp;
}

static void nv_forget_open_file(nv_linux_file_private_t *nvlfp)
{
    nv_linux_file_private_t **link;

    nvos_mutex_lock(&nv_open_files_lock);
    for (link = &nv_open_files; *link != NULL; link = &(*link)->next_open)
    {
        if (*link == nvlfp)
        {
            *link = nvlfp->next_open;
            break;
        }
    }
    nvos_mutex_unlock(&nv_open_files_lock);
}

void nvrm_close(nv_linux_file_private_t *nvlfp)
{
    nv_alloc_t *at, *next;
    nv_linux_state_t *nvl = nvlfp->nvptr;
    nv_state_t *nv = NV_STATE_PTR(nvl);
    nvidia_stack_t *sp = nvlfp->sp;

    nv_printf(NV_DBG_INFO, "NVRM: nvidia_ctl_close\n");

    nv_forget_open_file(nvlfp);

    nvos_sema_down(&nvl->ldata_lock);
    /* A GPU stays started (and NV_FLAG_OPEN) for nvrm's life; the control
     * device is open while any file of it is. */
    if (atomic64_dec_and_test(&nvl->usage_count) && (nv->flags & NV_FLAG_CONTROL))
    {
        nv->flags &= ~NV_FLAG_OPEN;
    }
    nvos_sema_up(&nvl->ldata_lock);

    rm_cleanup_file_private(sp, nv, &nvlfp->nvfp);

    if (nvlfp->mmap_context.alloc != NULL && nvlfp->mmap_context.valid)
    {
        at = nvlfp->mmap_context.alloc;
        /* NVIDIA's nv_alloc_release: drop the context's reference. */
        if (atomic64_dec_and_test(&at->usage_count))
        {
            atomic64_inc(&at->usage_count);
            nv_free_pages(nv, at->num_pages, at->flags.contig, at->cache_type, (void *)at);
        }
    }

    if (nvlfp->free_list != NULL)
    {
        at = nvlfp->free_list;
        while (at != NULL)
        {
            next = at->next;
            nv_free_pages(nv, at->num_pages,
                          at->flags.contig,
                          at->cache_type,
                          (void *)at);
            at = next;
        }
    }

    if (nvlfp->num_attached_gpus != 0)
    {
        /* No GPU can be attached before N1d (NV_ESC_ATTACH_GPUS_TO_FD). */
        NV_KFREE(nvlfp->attached_gpus, sizeof(NvU32) * nvlfp->num_attached_gpus);
        nvlfp->num_attached_gpus = 0;
    }

    nv_free_file_private(nvlfp);
    nv_kmem_cache_free_stack(sp);
}

/* ------------------------------------------------------------------------
 * The dispatcher. NVIDIA's nvidia_read_card_info and nvidia_ioctl. The
 * changes: the argument is copied with the client's copy (os_memcpy_*_user,
 * the request bridge at N1e), not copy_from_user; the GPU's open-completion
 * wait and the device-only escapes (QUERY_DEVICE_INTR, NUMA_INFO,
 * SET_NUMA_STATUS, EXPORT_TO_DMABUF_FD) come with the GPU's files at N1d,
 * and refuse here as NV_ACTUAL_DEVICE_ONLY refuses them on the control
 * device.
 * ---------------------------------------------------------------------- */

#define NV_CTL_DEVICE_ONLY(nv)                 \
{                                              \
    if (((nv)->flags & NV_FLAG_CONTROL) == 0)  \
    {                                          \
        status = -EINVAL;                      \
        goto done;                             \
    }                                          \
}

static int nvidia_read_card_info(nv_ioctl_card_info_t *ci, size_t num_entries)
{
    nv_state_t *nv;
    nv_linux_state_t *nvl;
    size_t i = 0;
    int rc = 0;

    /* Clear each card's flags field the lazy way */
    memset(ci, 0, num_entries * sizeof(ci[0]));

    LOCK_NV_LINUX_DEVICES();

    if (num_entries < num_nv_devices)
    {
        rc = -EINVAL;
        goto out;
    }

    for (nvl = nv_linux_devices; nvl && i < num_entries; nvl = nvl->next)
    {
        nv = NV_STATE_PTR(nvl);

        /* We do not include excluded GPUs in the list... */
        if ((nv->flags & NV_FLAG_EXCLUDE) != 0)
            continue;

        ci[i].valid              = NV_TRUE;
        ci[i].pci_info.domain    = nv->pci_info.domain;
        ci[i].pci_info.bus       = nv->pci_info.bus;
        ci[i].pci_info.slot      = nv->pci_info.slot;
        ci[i].pci_info.vendor_id = nv->pci_info.vendor_id;
        ci[i].pci_info.device_id = nv->pci_info.device_id;
        ci[i].gpu_id             = nv->gpu_id;
        ci[i].interrupt_line     = nv->interrupt_line;
        ci[i].reg_address        = nv->regs->cpu_address;
        ci[i].reg_size           = nv->regs->size;
        ci[i].minor_number       = nvl->minor_num;
        ci[i].fb_address         = nv->fb->cpu_address;
        ci[i].fb_size            = nv->fb->size;
        i++;
    }

out:
    UNLOCK_NV_LINUX_DEVICES();
    return rc;
}

int nvrm_ioctl(nv_linux_file_private_t *nvlfp, unsigned int cmd, void *i_arg)
{
    NV_STATUS rmStatus;
    int status = 0;
    nv_linux_state_t *nvl;
    nv_state_t *nv;
    nvidia_stack_t *sp = NULL;
    nv_ioctl_xfer_t ioc_xfer;
    void *arg_ptr = i_arg;
    void *arg_copy = NULL;
    size_t arg_size = 0;
    int arg_cmd;

    nv_printf(NV_DBG_INFO, "NVRM: ioctl(0x%x, 0x%llx, 0x%x)\n",
        _IOC_NR(cmd), (unsigned long long)(NvUPtr)i_arg, _IOC_SIZE(cmd));

    arg_size = _IOC_SIZE(cmd);
    arg_cmd  = _IOC_NR(cmd);

    if (arg_cmd == NV_ESC_IOCTL_XFER_CMD)
    {
        if (arg_size != sizeof(nv_ioctl_xfer_t))
        {
            nv_printf(NV_DBG_ERRORS,
                    "NVRM: invalid ioctl XFER structure size!\n");
            status = -EINVAL;
            goto done_early;
        }

        if (os_memcpy_from_user(&ioc_xfer, arg_ptr, sizeof(ioc_xfer)) != NV_OK)
        {
            nv_printf(NV_DBG_ERRORS,
                    "NVRM: failed to copy in ioctl XFER data!\n");
            status = -EFAULT;
            goto done_early;
        }

        arg_cmd  = ioc_xfer.cmd;
        arg_size = ioc_xfer.size;
        arg_ptr  = NvP64_VALUE(ioc_xfer.ptr);

        if (arg_size > NV_ABSOLUTE_MAX_IOCTL_SIZE)
        {
            nv_printf(NV_DBG_ERRORS, "NVRM: invalid ioctl XFER size!\n");
            status = -EINVAL;
            goto done_early;
        }
    }

    NV_KMALLOC(arg_copy, arg_size ? arg_size : 1);
    if (arg_copy == NULL)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: failed to allocate ioctl memory\n");
        status = -ENOMEM;
        goto done_early;
    }

    if (os_memcpy_from_user(arg_copy, arg_ptr, arg_size) != NV_OK)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: failed to copy in ioctl data!\n");
        status = -EFAULT;
        goto done_early;
    }

    /*
     * Handle NV_ESC_WAIT_OPEN_COMPLETE early as it is allowed to work
     * with or without nvl.
     */
    if (arg_cmd == NV_ESC_WAIT_OPEN_COMPLETE)
    {
        nv_ioctl_wait_open_complete_t *params = arg_copy;

        if (arg_size != sizeof(nv_ioctl_wait_open_complete_t))
        {
            status = -EINVAL;
            goto done_early;
        }

        params->rc = nvlfp->open_rc;
        params->adapterStatus = nvlfp->adapter_status;
        goto done_early;
    }

    nvl = nvlfp->nvptr;
    if (nvl == NULL)
    {
        status = -EIO;
        goto done_early;
    }

    nv = NV_STATE_PTR(nvl);

    nvos_rwlock_read(&nv_system_pm_lock);

    status = nv_kmem_cache_alloc_stack(&sp);
    if (status != 0)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: Unable to allocate altstack for ioctl\n");
        goto done_pm_unlock;
    }

    switch (arg_cmd)
    {
        /* pass out info about the card */
        case NV_ESC_CARD_INFO:
        {
            size_t num_arg_devices = arg_size / sizeof(nv_ioctl_card_info_t);

            NV_CTL_DEVICE_ONLY(nv);

            status = nvidia_read_card_info(arg_copy, num_arg_devices);
            break;
        }

        case NV_ESC_ATTACH_GPUS_TO_FD:
        {
            size_t num_arg_gpus = arg_size / sizeof(NvU32);
            size_t i;

            NV_CTL_DEVICE_ONLY(nv);

            if ((num_arg_gpus == 0) || (arg_size % sizeof(NvU32) != 0))
            {
                status = -EINVAL;
                goto done;
            }

            /* atomically check and alloc attached_gpus */
            nvos_sema_down(&nvl->ldata_lock);

            if (nvlfp->num_attached_gpus != 0)
            {
                nvos_sema_up(&nvl->ldata_lock);
                status = -EINVAL;
                goto done;
            }

            /*
             * Ferrix: NVIDIA's nvidia_dev_get() looks each id up and opens
             * its GPU. nvrm's GPUs are started at probe (os/kept/nv-pci.c),
             * as a persistence-mode GPU is on Linux, so the lookup is all
             * that is left: an id no probed GPU has is refused.
             */
            for (i = 0; i < num_arg_gpus; i++)
            {
                if (!nv_gpu_id_known(((NvU32 *)arg_copy)[i]))
                {
                    nvos_sema_up(&nvl->ldata_lock);
                    status = -EINVAL;
                    goto done;
                }
            }

            NV_KMALLOC(nvlfp->attached_gpus, arg_size);
            if (nvlfp->attached_gpus == NULL)
            {
                nvos_sema_up(&nvl->ldata_lock);
                status = -ENOMEM;
                goto done;
            }
            memcpy(nvlfp->attached_gpus, arg_copy, arg_size);
            nvlfp->num_attached_gpus = num_arg_gpus;

            nvos_sema_up(&nvl->ldata_lock);
            break;
        }

        case NV_ESC_CHECK_VERSION_STR:
        {
            NV_CTL_DEVICE_ONLY(nv);

            rmStatus = rm_perform_version_check(sp, arg_copy, arg_size);
            status = ((rmStatus == NV_OK) ? 0 : -EINVAL);
            break;
        }

        case NV_ESC_SYS_PARAMS:
        {
            nv_ioctl_sys_params_t *api = arg_copy;

            NV_CTL_DEVICE_ONLY(nv);

            if (arg_size != sizeof(nv_ioctl_sys_params_t))
            {
                status = -EINVAL;
                goto done;
            }

            /* numa_memblock_size should only be set once */
            if (nvl->numa_memblock_size == 0)
            {
                nvl->numa_memblock_size = api->memblock_size;
            }
            else
            {
                status = (nvl->numa_memblock_size == api->memblock_size) ?
                    0 : -EBUSY;
                goto done;
            }
            break;
        }

        case NV_ESC_QUERY_DEVICE_INTR:
        case NV_ESC_NUMA_INFO:
        case NV_ESC_SET_NUMA_STATUS:
        case NV_ESC_EXPORT_TO_DMABUF_FD:
        {
            /* Device-only: the GPU's files come at N1d. */
            status = -EINVAL;
            break;
        }

        default:
            rmStatus = rm_ioctl(sp, nv, &nvlfp->nvfp, arg_cmd, arg_copy, arg_size);
            status = ((rmStatus == NV_OK) ? 0 : -EINVAL);
            break;
    }

done:
    nv_kmem_cache_free_stack(sp);

done_pm_unlock:
    nvos_rwlock_read_unlock(&nv_system_pm_lock);

done_early:
    if (arg_copy != NULL)
    {
        if (status != -EFAULT)
        {
            if (os_memcpy_to_user(arg_ptr, arg_copy, arg_size) != NV_OK)
            {
                nv_printf(NV_DBG_ERRORS, "NVRM: failed to copy out ioctl data\n");
                status = -EFAULT;
            }
        }
        NV_KFREE(arg_copy, arg_size);
    }

    return status;
}

/* ------------------------------------------------------------------------
 * System memory. NVIDIA's nv_alloc_pages and nv_free_pages, over
 * ferrix-nvos's pages (os/nvos/src/pages.rs) in place of alloc_pages and
 * dma_map_page: each page is pinned into the card's domain at allocation,
 * its device address doubling as its physical address (§4.3). Every page
 * is mapped in nvrm, contiguously, so nv_alloc_kernel_mapping is a lookup.
 * Caching is not encoded: on x86-64 the device snoops, and client mappings
 * take their caching at mmap (N1e).
 * ---------------------------------------------------------------------- */

NV_STATUS NV_API_CALL nv_alloc_pages(
    nv_state_t *nv,
    NvU32       page_count,
    NvU64       page_size,
    NvBool      contiguous,
    NvU32       cache_type,
    NvBool      zeroed,
    NvBool      unencrypted,
    NvS32       node_id,
    NvU64      *pte_array,
    void      **priv_data
)
{
    nv_alloc_t *at;
    NV_STATUS status;
    void *mapped = NULL;
    NvU64 *addresses;
    NvU32 i;

    nv_printf(NV_DBG_MEMINFO, "NVRM: VM: nv_alloc_pages: %d pages, nodeid %d\n", page_count, node_id);
    nv_printf(NV_DBG_MEMINFO, "NVRM: VM:    contig %d  cache_type %d\n",
        contiguous, cache_type);

    if (!contiguous && page_size == 0)
        return NV_ERR_INVALID_ARGUMENT;

    if (node_id != -1)
    {
        /* Ferrix: no NUMA node allocation (os_alloc_pages_node). */
        return NV_ERR_NOT_SUPPORTED;
    }

    at = nvos_create_alloc(page_count);
    if (at == NULL)
        return NV_ERR_NO_MEMORY;

    at->cache_type = cache_type;

    if (contiguous)
        at->flags.contig = NV_TRUE;
    if (zeroed)
        at->flags.zeroed = NV_TRUE;
    if (unencrypted)
        at->flags.unencrypted = NV_TRUE;

    addresses = calloc(page_count, sizeof(NvU64));
    if (addresses == NULL)
    {
        nvos_free_alloc(at);
        return NV_ERR_NO_MEMORY;
    }

    /*
     * Ferrix: a contiguous request asks ferrix-nvos for one run, which
     * writes its first address only; each page's follows from it. The check
     * below still holds the run to it.
     */
    status = nvos_pages_alloc(page_count, contiguous, addresses, &mapped, &at->pages);
    if (status != NV_OK)
    {
        free(addresses);
        nvos_free_alloc(at);
        return status;
    }
    if (contiguous)
    {
        for (i = 1; i < page_count; i++)
            addresses[i] = addresses[0] + ((NvU64)i << PAGE_SHIFT);
    }

    for (i = 0; i < page_count; i++)
    {
        at->page_table[i].phys_addr = addresses[i];
        at->page_table[i].virt_addr = (NvUPtr)mapped + ((NvUPtr)i << PAGE_SHIFT);
    }
    free(addresses);

    if (contiguous)
    {
        for (i = 1; i < page_count; i++)
        {
            if (at->page_table[i].phys_addr !=
                at->page_table[0].phys_addr + ((NvU64)i << PAGE_SHIFT))
            {
                nv_printf(NV_DBG_ERRORS,
                    "nvrm: a contiguous allocation of %u pages is not contiguous "
                    "(docs/NVIDIA.md §4.3)\n", page_count);
                nvos_pages_free(at->pages);
                nvos_free_alloc(at);
                return NV_ERR_NO_MEMORY;
            }
        }
    }

    if (zeroed)
        memset(mapped, 0, (size_t)page_count << PAGE_SHIFT);

    for (i = 0; i < ((contiguous) ? 1 : page_count); i++)
    {
        pte_array[i] = at->page_table[i].phys_addr;
    }

    *priv_data = at;
    atomic64_inc(&at->usage_count);

    return NV_OK;
}

NV_STATUS NV_API_CALL nv_free_pages(
    nv_state_t *nv,
    NvU32 page_count,
    NvBool contiguous,
    NvU32 cache_type,
    void *priv_data
)
{
    nv_alloc_t *at = priv_data;

    nv_printf(NV_DBG_MEMINFO, "NVRM: VM: nv_free_pages: 0x%x\n", page_count);

    /*
     * If the 'at' usage count doesn't drop to zero here, not all of
     * the user mappings have been torn down in time - we can't
     * safely free the memory. We report success back to the RM, but
     * defer the actual free operation until later.
     */
    if (!atomic64_dec_and_test(&at->usage_count))
        return NV_OK;

    if (!at->flags.guest && at->pages != NULL)
        nvos_pages_free(at->pages);

    nvos_free_alloc(at);

    return NV_OK;
}

void* NV_API_CALL nv_alloc_kernel_mapping(
    nv_state_t *nv,
    void       *pAllocPrivate,
    NvU64       pageIndex,
    NvU32       pageOffset,
    NvU64       size,
    void      **pPrivate
)
{
    nv_alloc_t *at = pAllocPrivate;

    /*
     * Ferrix: pages nvos allocated are mapped in nvrm already, one run for
     * the whole allocation, so any range of them is mapped. Pages RM was
     * only told the address of -- a client's, a guest's, physical ones --
     * have no mapping here: nvrm cannot map a physical address outside
     * its device's apertures.
     */
    if (at->pages == NULL || at->page_table[pageIndex].virt_addr == 0)
    {
        nv_printf(NV_DBG_ERRORS,
            "nvrm: nv_alloc_kernel_mapping: no mapping for pages RM did not "
            "allocate (user %d, guest %d, physical %d)\n",
            at->flags.user, at->flags.guest, at->flags.physical);
        return NULL;
    }

    if (pageIndex + ((pageOffset + size + PAGE_SIZE - 1) >> PAGE_SHIFT) > at->num_pages)
        return NULL;

    *pPrivate = NULL;
    return (void *)(at->page_table[pageIndex].virt_addr + pageOffset);
}

void NV_API_CALL nv_free_kernel_mapping(
    nv_state_t *nv,
    void       *pAllocPrivate,
    void       *address,
    void       *pPrivate
)
{
    /* Nothing was mapped by nv_alloc_kernel_mapping. */
}

/*
 * NVIDIA's nv_alias_pages, nv_register_peer_io_mem, nv_register_user_pages,
 * nv_register_phys_pages and their unregistering: bookkeeping of pages RM
 * did not allocate. Unchanged but for get_order, which nothing here needs.
 */
NV_STATUS NV_API_CALL nv_alias_pages(
    nv_state_t *nv,
    NvU32 page_cnt,
    NvU64 page_size,
    NvU32 contiguous,
    NvU32 cache_type,
    NvU64 guest_id,
    NvU64 *pte_array,
    NvBool carveout,
    void **priv_data
)
{
    nv_alloc_t *at;
    NvU32 i=0;
    nvidia_pte_t *page_ptr = NULL;

    at = nvos_create_alloc(page_cnt);

    if (at == NULL)
    {
        return NV_ERR_NO_MEMORY;
    }

    at->cache_type = cache_type;
    if (contiguous)
    {
        at->flags.contig = NV_TRUE;
    }

    at->flags.guest = NV_TRUE;
    at->flags.carveout = carveout;

    for (i=0; i < at->num_pages; ++i)
    {
        page_ptr = &at->page_table[i];

        if (contiguous && i>0)
        {
            page_ptr->phys_addr = pte_array[0] + (i << PAGE_SHIFT);
        }
        else
        {
            page_ptr->phys_addr  = pte_array[i];
        }

        /* aliased pages will be mapped on demand. */
        page_ptr->virt_addr = 0x0;
    }

    at->guest_id = guest_id;
    *priv_data = at;
    atomic64_inc(&at->usage_count);

    return NV_OK;
}

NV_STATUS NV_API_CALL nv_register_peer_io_mem(
    nv_state_t *nv,
    NvU64      *phys_addr,
    NvU64       page_count,
    void      **priv_data
)
{
    nv_alloc_t *at;
    NvU64 i;
    NvU64 addr;

    at = nvos_create_alloc(page_count);

    if (at == NULL)
        return NV_ERR_NO_MEMORY;

    // IO regions should be uncached and contiguous
    at->cache_type = NV_MEMORY_UNCACHED;
    at->flags.contig = NV_TRUE;
    at->flags.peer_io = NV_TRUE;

    addr = phys_addr[0];

    for (i = 0; i < page_count; i++)
    {
        at->page_table[i].phys_addr = addr;
        addr += PAGE_SIZE;
    }

    // No struct page array exists for this memory.
    at->user_pages = NULL;

    *priv_data = at;

    return NV_OK;
}

void NV_API_CALL nv_unregister_peer_io_mem(
    nv_state_t *nv,
    void       *priv_data
)
{
    nv_alloc_t *at = priv_data;

    nvos_free_alloc(at);
}

NV_STATUS NV_API_CALL nv_register_user_pages(
    nv_state_t *nv,
    NvU64       page_count,
    NvU64      *phys_addr,
    void       *import_priv,
    void      **priv_data,
    NvBool      unencrypted
)
{
    nv_alloc_t *at;
    NvU64 i;
    NvU64 *user_pages;

    nv_printf(NV_DBG_MEMINFO, "NVRM: VM: nv_register_user_pages: 0x%llx\n", page_count);

    /*
     * Ferrix: os_lock_user_pages (request_pin at N1e) hands over the
     * pinned pages' addresses, not struct pages.
     */
    user_pages = *priv_data;

    at = nvos_create_alloc(page_count);

    if (at == NULL)
    {
        return NV_ERR_NO_MEMORY;
    }

    /*
     * Anonymous memory currently must be write-back cacheable, and we can't
     * enforce contiguity.
     */
    at->cache_type = NV_MEMORY_UNCACHED;

    at->flags.user = NV_TRUE;

    if (unencrypted)
        at->flags.unencrypted = NV_TRUE;

    for (i = 0; i < page_count; i++)
    {
        /*
         * We only assign the physical address and not the DMA address, since
         * this allocation hasn't been DMA-mapped yet.
         */
        at->page_table[i].phys_addr = phys_addr[i] = user_pages[i];
    }

    /* Save off the user pages array to be restored later */
    at->user_pages = user_pages;

    /* Save off the import private data to be returned later */
    if (import_priv != NULL)
    {
        at->import_priv = import_priv;
    }

    *priv_data = at;

    return NV_OK;
}

void NV_API_CALL nv_unregister_user_pages(
    nv_state_t *nv,
    NvU64       page_count,
    void      **import_priv,
    void      **priv_data
)
{
    nv_alloc_t *at = *priv_data;

    nv_printf(NV_DBG_MEMINFO, "NVRM: VM: nv_unregister_user_pages: 0x%llx\n", page_count);

    /* Restore the user pages array for the caller to handle */
    *priv_data = at->user_pages;

    /* Return the import private data for the caller to handle */
    if (import_priv != NULL)
    {
        *import_priv = at->import_priv;
    }

    nvos_free_alloc(at);
}

NV_STATUS NV_API_CALL nv_register_phys_pages(
    nv_state_t *nv,
    NvU64      *phys_addr,
    NvU64       page_count,
    NvU32       cache_type,
    void      **priv_data
)
{
    nv_alloc_t *at;
    NvU64 i;

    at = nvos_create_alloc(page_count);

    if (at == NULL)
        return NV_ERR_NO_MEMORY;
    /*
     * Setting memory flags to cacheable and discontiguous.
     */
    at->cache_type = cache_type;

    /*
     * Only physical address is available so we don't try to reuse existing
     * mappings
     */
    at->flags.physical = NV_TRUE;

    for (i = 0; i < page_count; i++)
    {
        at->page_table[i].phys_addr = phys_addr[i];
    }

    at->user_pages = NULL;
    *priv_data = at;

    return NV_OK;
}

void NV_API_CALL nv_unregister_phys_pages(
    nv_state_t *nv,
    void       *priv_data
)
{
    nv_alloc_t *at = priv_data;

    nvos_free_alloc(at);
}

NV_STATUS NV_API_CALL nv_get_num_phys_pages(
    void    *pAllocPrivate,
    NvU32   *pNumPages
)
{
    nv_alloc_t *at = pAllocPrivate;

    if (!pNumPages) {
        return NV_ERR_INVALID_ARGUMENT;
    }

    *pNumPages = at->num_pages;

    return NV_OK;
}

/* ------------------------------------------------------------------------
 * Events. NVIDIA's nv_post_event and nv_get_event; the file's spinlock and
 * wait queue are ferrix-nvos's.
 * ---------------------------------------------------------------------- */

void NV_API_CALL nv_post_event(
    nv_event_t *event,
    NvHandle    handle,
    NvU32       index,
    NvU32       info32,
    NvU16       info16,
    NvBool      data_valid
)
{
    nv_linux_file_private_t *nvlfp = nv_get_nvlfp_from_nvfp(event->nvfp);
    nvidia_event_t *nvet;

    nvos_spin_lock(&nvlfp->fp_lock);

    if (data_valid)
    {
        NV_KMALLOC(nvet, sizeof(nvidia_event_t));
        if (nvet == NULL)
        {
            nvos_spin_unlock(&nvlfp->fp_lock);
            return;
        }

        if (nvlfp->event_data_tail != NULL)
            nvlfp->event_data_tail->next = nvet;
        if (nvlfp->event_data_head == NULL)
            nvlfp->event_data_head = nvet;
        nvlfp->event_data_tail = nvet;
        nvet->next = NULL;

        nvet->event = *event;
        nvet->event.hObject = handle;
        nvet->event.index = index;
        nvet->event.info32 = info32;
        nvet->event.info16 = info16;
    }
    //
    // 'event_pending' is interpreted by nvidia_poll() and nv_get_event() to
    // mean that an event without data is pending. Therefore, only set it to
    // true here if newly posted event is dataless.
    //
    else
    {
        nvlfp->dataless_event_pending = NV_TRUE;
    }

    nvos_spin_unlock(&nvlfp->fp_lock);

    /* N1e turns this into readiness on the file (§4.4, "Events"). */
    nvos_event_signal(&nvlfp->waitqueue);
}

int NV_API_CALL nv_get_event(
    nv_file_private_t  *nvfp,
    nv_event_t         *event,
    NvU32              *pending
)
{
    nv_linux_file_private_t *nvlfp = nv_get_nvlfp_from_nvfp(nvfp);
    nvidia_event_t *nvet;

    nvos_spin_lock(&nvlfp->fp_lock);

    nvet = nvlfp->event_data_head;
    if (nvet == NULL)
    {
        nvos_spin_unlock(&nvlfp->fp_lock);
        return NV_ERR_GENERIC;
    }

    *event = nvet->event;

    if (nvlfp->event_data_tail == nvet)
        nvlfp->event_data_tail = NULL;
    nvlfp->event_data_head = nvet->next;

    *pending = (nvlfp->event_data_head != NULL);

    nvos_spin_unlock(&nvlfp->fp_lock);

    NV_KFREE(nvet, sizeof(nvidia_event_t));

    return NV_OK;
}

/*
 * NVIDIA's nv_get_file_private: the file a client's descriptor names. On
 * Linux, fget(fd) in the calling process. In nvrm, N1e's request_file
 * resolves the client's descriptor to a file identity of the forwarding
 * core; until then the identities are the pseudo-descriptors nvrm_open_ctl
 * handed out, which is what nvrm-link-test passes.
 */
nv_file_private_t* NV_API_CALL nv_get_file_private(
    NvS32 fd,
    NvBool ctl,
    void **os_private
)
{
    nv_linux_file_private_t *nvlfp;

    nvos_mutex_lock(&nv_open_files_lock);
    for (nvlfp = nv_open_files; nvlfp != NULL; nvlfp = nvlfp->next_open)
    {
        if (nvlfp->fd == fd)
            break;
    }
    nvos_mutex_unlock(&nv_open_files_lock);

    if (nvlfp == NULL)
        return NULL;

    if (ctl != !!NV_IS_CTL_DEVICE(NV_STATE_PTR(nvlfp->nvptr)))
        return NULL;

    *os_private = nvlfp;

    return &nvlfp->nvfp;
}

void NV_API_CALL nv_put_file_private(
    void *os_private
)
{
    /* Nothing was referenced: files live until nvrm_close. */
}

/* NVIDIA's nv_match_gpu_os_info: whether a file is this GPU's. */
NvBool NV_API_CALL nv_match_gpu_os_info(nv_state_t *nv, void *os_info)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);
    nv_linux_file_private_t *nvlfp = os_info;

    return (nvlfp != NULL) && (nvlfp->nvptr == nvl);
}

/* ------------------------------------------------------------------------
 * The RC timer. NVIDIA's nv_start_rc_timer, nv_stop_rc_timer and
 * nvidia_rc_timer_callback, on a ferrix-nvos timer: once a second, RM's
 * RC callback, then the timer again unless it failed.
 * ---------------------------------------------------------------------- */

#define NV_RC_TIMER_NS 1000000000ULL

static void nvidia_rc_timer_callback(void *data)
{
    nv_linux_state_t *nvl = data;
    nv_state_t *nv = NV_STATE_PTR(nvl);
    nvidia_stack_t *sp = NULL;

    if (nv_kmem_cache_alloc_stack(&sp) != 0)
        return;

    if (rm_run_rc_callback(sp, nv) == NV_OK)
    {
        // set another timeout 1 sec in the future:
        if (nv->rc_timer_enabled)
            nvos_timer_start(nvl->rc_timer, NV_RC_TIMER_NS);
    }

    nv_kmem_cache_free_stack(sp);
}

int NV_API_CALL nv_start_rc_timer(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    if (nv->rc_timer_enabled)
        return -1;

    nv_printf(NV_DBG_INFO, "NVRM: initializing rc timer\n");

    if (nvl->rc_timer == NULL)
        nvl->rc_timer = nvos_timer_create(nvidia_rc_timer_callback, nvl);
    if (nvl->rc_timer == NULL)
        return -1;

    nv->rc_timer_enabled = 1;

    // set the timeout for 1 second in the future:
    nvos_timer_start(nvl->rc_timer, NV_RC_TIMER_NS);

    nv_printf(NV_DBG_INFO, "NVRM: rc timer initialized\n");

    return 0;
}

int NV_API_CALL nv_stop_rc_timer(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    if (!nv->rc_timer_enabled)
        return -1;

    nv_printf(NV_DBG_INFO, "NVRM: stopping rc timer\n");
    nv->rc_timer_enabled = 0;
    nvos_timer_cancel(nvl->rc_timer);
    nv_printf(NV_DBG_INFO, "NVRM: rc timer stopped\n");

    return 0;
}

/* ------------------------------------------------------------------------
 * Firmware. NVIDIA's nv_get_firmware names the file as request_firmware
 * would, relative to /lib/firmware; nvrm reads it from there, or from the
 * NVIDIA volume mounted at /data (docs/NVIDIA.md §3).
 * ---------------------------------------------------------------------- */

typedef struct
{
    void *data;
    NvU64 size;
} nv_firmware_blob_t;

const void* NV_API_CALL nv_get_firmware(
    nv_state_t *nv,
    nv_firmware_type_t fw_type,
    nv_firmware_chip_family_t fw_chip_family,
    const void **fw_buf,
    NvU32 *fw_size
)
{
    static const char *const roots[] = { "/lib/firmware/", "/data/lib/firmware/" };
    const char *name = nv_firmware_for_chip_family(fw_type, fw_chip_family);
    nv_firmware_blob_t *blob;
    char path[256];
    size_t i;

    if (name == NULL || name[0] == '\0')
        return NULL;

    NV_KZALLOC(blob, sizeof(*blob));
    if (blob == NULL)
        return NULL;

    for (i = 0; i < sizeof(roots) / sizeof(roots[0]); i++)
    {
        snprintf(path, sizeof(path), "%s%s", roots[i], name);
        blob->data = nvos_read_whole_file(path, &blob->size);
        if (blob->data != NULL)
            break;
    }

    if (blob->data == NULL || blob->size > 0xffffffffULL)
    {
        nv_printf(NV_DBG_ERRORS, "nvrm: firmware %s not found\n", name);
        free(blob->data);
        NV_KFREE(blob, sizeof(*blob));
        return NULL;
    }

    *fw_size = (NvU32)blob->size;
    *fw_buf = blob->data;

    return blob;
}

void NV_API_CALL nv_put_firmware(
    const void *fw_handle
)
{
    nv_firmware_blob_t *blob = (nv_firmware_blob_t *)fw_handle;

    if (blob == NULL)
        return;
    free(blob->data);
    NV_KFREE(blob, sizeof(*blob));
}

/* ------------------------------------------------------------------------
 * The rest of nv.c's calls, each NVIDIA's, with what a discrete GPU in
 * nvrm answers.
 * ---------------------------------------------------------------------- */

nv_state_t* NV_API_CALL nv_get_adapter_state(
    NvU32 domain,
    NvU8  bus,
    NvU8  slot
)
{
    nv_linux_state_t *nvl;

    LOCK_NV_LINUX_DEVICES();
    for (nvl = nv_linux_devices; nvl != NULL;  nvl = nvl->next)
    {
        nv_state_t *nv = NV_STATE_PTR(nvl);
        if (nv->pci_info.domain == domain && nv->pci_info.bus == bus
            && nv->pci_info.slot == slot)
        {
            UNLOCK_NV_LINUX_DEVICES();
            return nv;
        }
    }
    UNLOCK_NV_LINUX_DEVICES();

    return NULL;
}

nv_state_t* NV_API_CALL nv_get_ctl_state(void)
{
    return NV_STATE_PTR(&nv_ctl_device);
}

NvU32 NV_API_CALL nv_get_dev_minor(nv_state_t *nv)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    return nvl->minor_num;
}

/* NVIDIA's sets the DMA mask; pins are reached by the IOMMU, so it is kept. */
void NV_API_CALL nv_set_dma_address_size(
    nv_state_t  *nv,
    NvU32       phys_addr_bits
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    nvl->dma_mask = (((NvU64)1) << phys_addr_bits) - 1;
}

/* Linux: true unless SWIOTLB bounces. Pins are never bounced. */
NvBool NV_API_CALL nv_requires_dma_remap(
    nv_state_t *nv
)
{
    return NV_TRUE;
}

/* Linux reads the ROM's shadow flag; nvrm's GPU is never the boot VGA. */
NV_STATUS NV_API_CALL nv_set_primary_vga_status(
    nv_state_t *nv
)
{
    nv->primary_vga = NV_FALSE;
    return NV_OK;
}

/* No firmware framebuffer console on the GPU (it is not the boot VGA). */
void NV_API_CALL nv_get_screen_info(
    nv_state_t  *nv,
    NvU64       *pPhysicalAddress,
    NvU32       *pFbWidth,
    NvU32       *pFbHeight,
    NvU32       *pFbDepth,
    NvU32       *pFbPitch,
    NvU64       *pFbSize
)
{
    *pPhysicalAddress = 0;
    *pFbWidth = *pFbHeight = *pFbDepth = *pFbPitch = 0;
    *pFbSize = 0;
}

/* Linux narrows the VGA emulator's segment to the iomem tree's; nvrm has
 * no iomem tree, and leaves it as asked. */
void NV_API_CALL nv_get_updated_emu_seg(
    NvU32 *start,
    NvU32 *end
)
{
}

/* Coherent (Grace) platforms only. */
NV_STATUS NV_API_CALL nv_get_device_memory_config(
    nv_state_t *nv,
    NvU64 *compr_addr_sys_phys,
    NvU64 *addr_guest_phys,
    NvU64 *size_guest_phys,
    NvU64 *rsvd_phys,
    NvU32 *addr_width,
    NvS32 *node_id
)
{
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_get_egm_info(
    nv_state_t *nv,
    NvU64 *phys_addr,
    NvU64 *size,
    NvS32 *egm_node_id
)
{
    return NV_ERR_NOT_SUPPORTED;
}

/* No ACPI tables or /sys/power in nvrm: no S0ix, not configured. */
NvBool NV_API_CALL nv_platform_supports_s0ix(void)
{
    return NV_FALSE;
}

NvBool NV_API_CALL nv_s2idle_pm_configured(void)
{
    return NV_FALSE;
}

/* No DMI in nvrm: not known to be a notebook. */
NvBool NV_API_CALL nv_is_chassis_notebook(void)
{
    return NV_FALSE;
}

/* Runtime power management: off, as Linux without NV_PM_RUNTIME_AVAILABLE. */
NvBool NV_API_CALL nv_dynamic_power_available(nv_state_t *nv)
{
    return NV_FALSE;
}

NV_STATUS NV_API_CALL nv_indicate_idle(nv_state_t *nv)
{
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_indicate_not_idle(nv_state_t *nv)
{
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_idle_holdoff(nv_state_t *nv)
{
}

void NV_API_CALL nv_allow_runtime_suspend(nv_state_t *nv)
{
}

void NV_API_CALL nv_disallow_runtime_suspend(nv_state_t *nv)
{
}

void NV_API_CALL nv_audio_dynamic_power(nv_state_t *nv)
{
}

/* UVM is not loaded beside RM until N5 (§11). */
void NV_API_CALL nv_schedule_uvm_isr(nv_state_t *nv)
{
}

NV_STATUS NV_API_CALL nv_schedule_uvm_drain_p2p(NvU8 *pUuid)
{
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_schedule_uvm_resume_p2p(NvU8 *pUuid)
{
}

/* AArch64 only in NVIDIA's; x86-64 is coherent. */
void NV_API_CALL nv_flush_coherent_cpu_cache_range(nv_state_t *nv, NvU64 cpu_virtual, NvU64 size)
{
}

/* NVIDIA's nv_log_error reports through nv_report_error to the kernel log. */
NV_STATUS NV_API_CALL nv_log_error(
    nv_state_t *nv,
    NvU32       error_number,
    const char *format,
    va_list    ap
)
{
    char text[512];

    vsnprintf(text, sizeof(text), format, ap);
    nv_printf(NV_DBG_ERRORS, "NVRM: GPU " NV_PCI_DEV_FMT ": Xid %u: %s\n",
              NV_PCI_DEV_FMT_ARGS(nv), error_number, text);
    return NV_OK;
}
