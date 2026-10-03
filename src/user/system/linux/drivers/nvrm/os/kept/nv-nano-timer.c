/*
 * SPDX-FileCopyrightText: Copyright (c) 2020-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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
 * kernel-open/nvidia/nv-nano-timer.c at 580.173.02, under the MIT licence
 * above: RM's nanosecond timers. Changes: the hrtimer is a ferrix-nvos
 * timer (os/nvos/src/timer.rs), whose callback runs on the timer thread
 * rather than in soft-IRQ; the callback into RM is NVIDIA's, unchanged.
 */

#define  __NO_VERSION__

#include "os-interface.h"
#include "nv-ferrix.h"

struct nv_nano_timer
{
    struct nvos_timer *timer;
    nv_linux_state_t *nv_linux_state;
    void (*nv_nano_timer_callback)(struct nv_nano_timer *nv_nstimer);
    void *pTmrEvent;
};

/*!
 * @brief runs nano second resolution timer callback 
*
 * @param[in] nv_nstimer    Pointer to nv_nano_timer_t object
 */
static void
nvidia_nano_timer_callback(
    nv_nano_timer_t *nv_nstimer)
{
    nv_state_t *nv = NULL;
    nv_linux_state_t *nvl = nv_nstimer->nv_linux_state;
    nvidia_stack_t *sp = NULL;

    if (nv_kmem_cache_alloc_stack(&sp) != 0)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: no cache memory \n");
        return;
    }

    nv = NV_STATE_PTR(nvl);

    if (rm_run_nano_timer_callback(sp, nv, nv_nstimer->pTmrEvent) != NV_OK)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: Error in service of callback \n");
    }

    nv_kmem_cache_free_stack(sp);
}

/* The timer thread's call: Linux's nv_nano_timer_callback_typed_data. */
static void nv_nano_timer_callback_typed_data(void *data)
{
    struct nv_nano_timer *nv_nstimer = data;

    nv_nstimer->nv_nano_timer_callback(nv_nstimer);
}

/*!
 * @brief Creates & initializes nano second resolution timer object
 *
 * @param[in] nv           Per gpu linux state
 * @param[in] tmrEvent     pointer to TMR_EVENT
 * @param[in] nv_nstimer    Pointer to nv_nano_timer_t object
 */
void NV_API_CALL nv_create_nano_timer(
    nv_state_t *nv,
    void *pTmrEvent,
    nv_nano_timer_t **pnv_nstimer)
{
    nv_linux_state_t *nvl = NV_GET_NVL_FROM_NV_STATE(nv);
    nv_nano_timer_t *nv_nstimer;

    NV_KZALLOC(nv_nstimer, sizeof(nv_nano_timer_t));
    if (nv_nstimer == NULL)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: Not able to create timer object \n");
        *pnv_nstimer = NULL;
        return;
    }

    nv_nstimer->nv_linux_state = nvl;
    nv_nstimer->pTmrEvent = pTmrEvent;
    nv_nstimer->nv_nano_timer_callback = nvidia_nano_timer_callback;
    nv_nstimer->timer = nvos_timer_create(nv_nano_timer_callback_typed_data, nv_nstimer);
    if (nv_nstimer->timer == NULL)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: Not able to create timer object \n");
        NV_KFREE(nv_nstimer, sizeof(nv_nano_timer_t));
        *pnv_nstimer = NULL;
        return;
    }

    *pnv_nstimer = nv_nstimer;
}

/*!
 * @brief Starts nano second resolution timer
 *
 * @param[in] nv           Per gpu linux state
 * @param[in] nv_nstimer   Pointer to nv_nano_timer_t object
 * @param[in] time_ns      Relative time in nano seconds
 */
void NV_API_CALL nv_start_nano_timer(
    nv_state_t *nv,
    nv_nano_timer_t *nv_nstimer,
    NvU64 time_ns)
{
    nvos_timer_start(nv_nstimer->timer, time_ns);
}

/*!
 * @brief Cancels nano second resolution timer
 *
 * @param[in] nv           Per gpu linux state
 * @param[in] nv_nstimer   Pointer to nv_nano_timer_t object
 */
void NV_API_CALL nv_cancel_nano_timer(
    nv_state_t *nv,
    nv_nano_timer_t *nv_nstimer)
{
    nvos_timer_cancel(nv_nstimer->timer);
}

/*!
 * @brief Cancels & deletes nano second resolution timer object
 *
 * @param[in] nv           Per gpu linux state
 * @param[in] nv_nstimer   Pointer to nv_nano_timer_t object
 */
void NV_API_CALL nv_destroy_nano_timer(
    nv_state_t *nv,
    nv_nano_timer_t *nv_nstimer)
{
    nvos_timer_destroy(nv_nstimer->timer);
    NV_KFREE(nv_nstimer, sizeof(nv_nano_timer_t));
}
