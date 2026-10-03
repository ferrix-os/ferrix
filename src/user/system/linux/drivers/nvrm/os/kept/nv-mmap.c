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
 * kernel-open/nvidia/nv-mmap.c at 580.173.02, under the MIT licence above:
 * the five calls RM makes of it, the mmap lock and the revocation state.
 * The rest of nv-mmap.c -- the mmap and fault handlers -- is the
 * forwarding core's at N1e, and the revocation itself is the message that
 * makes the kernel unmap every client mapping of a file (§4.4). Changes:
 * the semaphore is ferrix-nvos's; the per-file unmap_mapping_range waits
 * for that message, and is said here when it is skipped.
 */

#define  __NO_VERSION__

#include "os-interface.h"
#include "nv-ferrix.h"

static void
nv_revoke_gpu_mappings_locked(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    /* Revoke all mappings for every open file: N1e's REVOKE message. */
    nvos_stub_called("nv_revoke_gpu_mappings", "client mappings are revoked by N1e's forwarding core");

    nvl->all_mappings_revoked = NV_TRUE;
}

NV_STATUS NV_API_CALL nv_revoke_gpu_mappings(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    // Mapping revocation is only supported for GPU mappings.
    if (NV_IS_CTL_DEVICE(nv))
    {
        return NV_ERR_NOT_SUPPORTED;
    }

    nvos_sema_down(&nvl->mmap_lock);

    nv_revoke_gpu_mappings_locked(nv);

    nvos_sema_up(&nvl->mmap_lock);

    return NV_OK;
}

void NV_API_CALL nv_acquire_mmap_lock(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);
    nvos_sema_down(&nvl->mmap_lock);
}

void NV_API_CALL nv_release_mmap_lock(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);
    nvos_sema_up(&nvl->mmap_lock);
}

NvBool NV_API_CALL nv_get_all_mappings_revoked_locked(
    nv_state_t *nv
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    // Caller must hold nvl->mmap_lock for all decisions based on this
    return nvl->all_mappings_revoked;
}

void NV_API_CALL nv_set_safe_to_mmap_locked(
    nv_state_t *nv,
    NvBool safe_to_mmap
)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);

    // Caller must hold nvl->mmap_lock

    /*
     * Ferrix: NVIDIA's also notes that a fault must wake the GPU when this
     * goes from true to false; faults are N1e's.
     */
    nvl->safe_to_mmap = safe_to_mmap;
}
