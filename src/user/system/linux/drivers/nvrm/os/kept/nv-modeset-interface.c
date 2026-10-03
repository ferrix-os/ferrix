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
 * Kept for Ferrix (docs/NVIDIA.md §4.6) from NVIDIA's
 * kernel-open/nvidia/nv-modeset-interface.c at 580.173.02, under the MIT
 * licence above: the table of RM calls nvidia-modeset (NVKMS) makes, and
 * the callbacks it registers. On Linux the table crosses from nvidia.ko to
 * nvidia-modeset.ko; here NVKMS is linked into nvrm's core beside RM, so
 * it is an ordinary call. Changes: the device list is walked by nv.c's
 * nv_linux_devices_each, and NUMA onlining is never needed, as nvrm does
 * not online GPU memory (§4.5).
 */

#include "nv-modeset-interface.h"

#include "os-interface.h"
#include "nv-ferrix.h"
#include "nvstatus.h"
#include "nv.h"

static const nvidia_modeset_callbacks_t *nv_modeset_callbacks;

static int nvidia_modeset_rm_ops_alloc_stack(nvidia_stack_t **sp)
{
    return nv_kmem_cache_alloc_stack(sp);
}

static void nvidia_modeset_rm_ops_free_stack(nvidia_stack_t *sp)
{
    if (sp != NULL)
    {
        nv_kmem_cache_free_stack(sp);
    }
}

static int nvidia_modeset_set_callbacks(const nvidia_modeset_callbacks_t *cb)
{
    if ((nv_modeset_callbacks != NULL && cb != NULL) ||
        (nv_modeset_callbacks == NULL && cb == NULL))
    {
        return -EINVAL;
    }

    nv_modeset_callbacks = cb;
    return 0;
}

void nvidia_modeset_suspend(NvU32 gpuId)
{
    if (nv_modeset_callbacks)
    {
        nv_modeset_callbacks->suspend(gpuId);
    }
}

void nvidia_modeset_resume(NvU32 gpuId)
{
    if (nv_modeset_callbacks)
    {
        nv_modeset_callbacks->resume(gpuId);
    }
}

void nvidia_modeset_remove(NvU32 gpuId)
{
    if (nv_modeset_callbacks && nv_modeset_callbacks->remove)
    {
        nv_modeset_callbacks->remove(gpuId);
    }
}

static void nvidia_modeset_get_gpu_info(nv_gpu_info_t *gpu_info,
                                        const nv_linux_state_t *nvl)
{
    const nv_state_t *nv = NV_STATE_PTR(nvl);

    gpu_info->gpu_id = nv->gpu_id;

    gpu_info->pci_info.domain   = nv->pci_info.domain;
    gpu_info->pci_info.bus      = nv->pci_info.bus;
    gpu_info->pci_info.slot     = nv->pci_info.slot;
    gpu_info->pci_info.function = nv->pci_info.function;

    /* Ferrix: GPU memory is never onlined as a NUMA node. */
    gpu_info->needs_numa_setup = NV_FALSE;

    gpu_info->os_device_ptr = NULL;
}

void nvidia_modeset_probe(const nv_linux_state_t *nvl)
{
    if (nv_modeset_callbacks && nv_modeset_callbacks->probe)
    {
        nv_gpu_info_t gpu_info;

        nvidia_modeset_get_gpu_info(&gpu_info, nvl);
        nv_modeset_callbacks->probe(&gpu_info);
    }
}

static void nvidia_modeset_enumerate_one(const nv_linux_state_t *nvl, NvU32 index,
                                         void *argument)
{
    nv_gpu_info_t *gpu_info = argument;

    nvidia_modeset_get_gpu_info(&gpu_info[index], nvl);
}

static NvU32 nvidia_modeset_enumerate_gpus(nv_gpu_info_t *gpu_info)
{
    /*
     * The gpu_info[] array has NV_MAX_GPUS elements; the walk fails if
     * there are more GPUs than that.
     */
    return nv_linux_devices_each(NV_MAX_GPUS, nvidia_modeset_enumerate_one, gpu_info);
}

NV_STATUS nvidia_get_rm_ops(nvidia_modeset_rm_ops_t *rm_ops)
{
    const nvidia_modeset_rm_ops_t local_rm_ops = {
        .version_string = NV_VERSION_STRING,
        .system_info    = {
            .allow_write_combining = NV_FALSE,
        },
        .alloc_stack    = nvidia_modeset_rm_ops_alloc_stack,
        .free_stack     = nvidia_modeset_rm_ops_free_stack,
        .enumerate_gpus = nvidia_modeset_enumerate_gpus,
        .open_gpu       = nvidia_dev_get,
        .close_gpu      = nvidia_dev_put,
        .op             = rm_kernel_rmapi_op, /* provided by nv-kernel.o */
        .set_callbacks  = nvidia_modeset_set_callbacks,
    };

    if (strcmp(rm_ops->version_string, NV_VERSION_STRING) != 0)
    {
        rm_ops->version_string = NV_VERSION_STRING;
        return NV_ERR_GENERIC;
    }

    *rm_ops = local_rm_ops;

    /* nv-linux.h's NV_ALLOW_WRITE_COMBINING on x86-64 with the PAT, which
     * the kernel programs (N0c): allowed for the framebuffer. */
    rm_ops->system_info.allow_write_combining = NV_TRUE;

    return NV_OK;
}
