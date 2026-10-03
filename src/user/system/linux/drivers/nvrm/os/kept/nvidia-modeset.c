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
 * kernel-open/nvidia-modeset/nvidia-modeset-linux.c at 580.173.02, under
 * the MIT licence above: the OS layer NVKMS, NVIDIA's display driver
 * (nv-modeset-kernel.o), runs on. NVKMS is linked into nvrm's core beside
 * RM, so this is the whole of nvidia-modeset.ko that nvrm keeps.
 *
 * Changes:
 *  - locks, timers and the kthread queue are ferrix-nvos's: a semaphore,
 *    nvos_timer (whose thread stands in for the timer soft-IRQ) and a work
 *    queue;
 *  - there is no suspend on Ferrix, so the PM lock and the deferred-close
 *    queue that only a held PM lock needs are left out;
 *  - only kernel-space clients (KAPI) are served: /dev/nvidia-modeset, its
 *    poll and its per-open lookup by fd, procfs, the config file, backlight
 *    devices and Tegra syncpoints are not kept, and say so where NVKMS
 *    asks for them;
 *  - module parameters keep Linux's defaults as constants;
 *  - NVKMS is never unloaded in nvrm, so the pending-timer list that
 *    nvkms_exit cancels is not kept.
 */

#include "nv-ferrix.h"
#include "nv-modeset-interface.h"
#include "nvidia-modeset-os-interface.h"
#include "nvkms.h"

void *memmove(void *, const void *, size_t);
char *strncpy(char *, const char *, size_t);

#define NVKMS_LOG_PREFIX "nvidia-modeset: "

/* The module parameters, at Linux's defaults. */
static const NvBool output_rounding_fix = NV_TRUE;
static const NvBool disable_hdmi_frl = NV_FALSE;
static const NvBool disable_vrr_memclk_switch = NV_FALSE;
static const NvBool hdmi_deepcolor = NV_TRUE;
static const NvBool vblank_sem_control = NV_TRUE;
static const NvBool opportunistic_display_sync = NV_TRUE;
static const enum NvKmsDebugForceColorSpace debug_force_color_space =
    NVKMS_DEBUG_FORCE_COLOR_SPACE_NONE;
static const NvBool enable_overlay_layers = NV_TRUE;
static const NvBool conceal_vrr_caps = NV_FALSE;
static const int fail_alloc_core_channel_method = -1;
static const int debug = 0;

NvBool nvkms_test_fail_alloc_core_channel(
    enum FailAllocCoreChannelMethod method
)
{
    if ((int)method != fail_alloc_core_channel_method) {
        // don't fail if it's not the currently specified method
        return NV_FALSE;
    }

    return NV_TRUE;
}

NvBool nvkms_conceal_vrr_caps(void)
{
    return conceal_vrr_caps;
}

NvBool nvkms_output_rounding_fix(void)
{
    return output_rounding_fix;
}

NvBool nvkms_disable_hdmi_frl(void)
{
    return disable_hdmi_frl;
}

NvBool nvkms_disable_vrr_memclk_switch(void)
{
    return disable_vrr_memclk_switch;
}

NvBool nvkms_hdmi_deepcolor(void)
{
    return hdmi_deepcolor;
}

NvBool nvkms_vblank_sem_control(void)
{
    return vblank_sem_control;
}

NvBool nvkms_opportunistic_display_sync(void)
{
    return opportunistic_display_sync;
}

enum NvKmsDebugForceColorSpace nvkms_debug_force_color_space(void)
{
    if (debug_force_color_space >= NVKMS_DEBUG_FORCE_COLOR_SPACE_MAX) {
        return NVKMS_DEBUG_FORCE_COLOR_SPACE_NONE;
    }
    return debug_force_color_space;
}

NvBool nvkms_enable_overlay_layers(void)
{
    return enable_overlay_layers;
}

NvBool nvkms_debug_logging(void)
{
    return debug != 0;
}

NvBool nvkms_kernel_supports_syncpts(void)
{
    return NV_FALSE;
}

/* Ferrix: no Tegra host1x, as on a Linux without it (NVKMS_SYNCPT_STUBS_NEEDED). */
NvBool nvkms_syncpt_op(
    enum NvKmsSyncPtOp op,
    NvKmsSyncPtOpParams *params)
{
    return NV_FALSE;
}

/*************************************************************************
 * NVKMS uses a global lock, nvkms_lock.  The lock is taken in the
 * file operation callback functions when calling into core NVKMS.
 *************************************************************************/

static nvos_sema_t nvkms_lock;

/*************************************************************************
 * The kthread queue every timer and KAPI event runs on, in process context.
 *************************************************************************/

static void *nvkms_kthread_q;

static void nvkms_queue_work(void (*run)(void *), void *argument)
{
    NvBool ret = nvos_work_queue_schedule(nvkms_kthread_q, run, argument);
    /*
     * Ferrix: nvos_work_queue_schedule fails only when it cannot allocate
     * the item; Linux's fails only for an item already queued.
     */
    if (!ret) {
        nv_printf(NV_DBG_ERRORS, NVKMS_LOG_PREFIX "work item lost\n");
    }
}

/*************************************************************************
 * Interface with core NVKMS.
 *************************************************************************/

void* nvkms_alloc(size_t size, NvBool zero)
{
    if (size == 0) {
        return NULL;
    }

    return zero ? calloc(1, size) : malloc(size);
}

void nvkms_free(void *ptr, size_t size)
{
    free(ptr);
}

void* nvkms_memset(void *ptr, NvU8 c, size_t size)
{
    return memset(ptr, c, size);
}

void* nvkms_memcpy(void *dest, const void *src, size_t n)
{
    return memcpy(dest, src, n);
}

void* nvkms_memmove(void *dest, const void *src, size_t n)
{
    return memmove(dest, src, n);
}

int nvkms_memcmp(const void *s1, const void *s2, size_t n)
{
    return memcmp(s1, s2, n);
}

size_t nvkms_strlen(const char *s)
{
    return strlen(s);
}

int nvkms_strcmp(const char *s1, const char *s2)
{
    return strcmp(s1, s2);
}

char* nvkms_strncpy(char *dest, const char *src, size_t n)
{
    return strncpy(dest, src, n);
}

/* Ferrix: RM's own delay, which sleeps for waits longer than a tick. */
void nvkms_usleep(NvU64 usec)
{
    while (usec > 0) {
        NvU32 step = usec > 1000000 ? 1000000 : (NvU32)usec;
        (void)os_delay_us(step);
        usec -= step;
    }
}

NvU64 nvkms_get_usec(void)
{
    return os_get_monotonic_time_ns() / 1000;
}

/*
 * Ferrix: for user-space clients only, which nvrm does not serve yet;
 * RM's copy, through the client the calling thread serves.
 */
int nvkms_copyin(void *kptr, NvU64 uaddr, size_t n)
{
    if (n > 0xffffffffu ||
        os_memcpy_from_user(kptr, (const void *)(NvUPtr)uaddr, (NvU32)n) != NV_OK) {
        return -EFAULT;
    }
    return 0;
}

int nvkms_copyout(NvU64 uaddr, const void *kptr, size_t n)
{
    if (n > 0xffffffffu ||
        os_memcpy_to_user((void *)(NvUPtr)uaddr, kptr, (NvU32)n) != NV_OK) {
        return -EFAULT;
    }
    return 0;
}

void nvkms_yield(void)
{
    (void)os_schedule();
}

int nvkms_snprintf(char *str, size_t size, const char *format, ...)
{
    int ret;
    va_list ap;

    va_start(ap, format);
    ret = vsnprintf(str, size, format, ap);
    va_end(ap);

    return ret;
}

int nvkms_vsnprintf(char *str, size_t size, const char *format, va_list ap)
{
    return vsnprintf(str, size, format, ap);
}

void nvkms_log(const int level, const char *gpuPrefix, const char *msg)
{
    const char *levelPrefix;

    switch (level) {
    default:
    case NVKMS_LOG_LEVEL_INFO:
        levelPrefix = "";
        break;
    case NVKMS_LOG_LEVEL_WARN:
        levelPrefix = "WARNING: ";
        break;
    case NVKMS_LOG_LEVEL_ERROR:
        levelPrefix = "ERROR: ";
        break;
    }

    nv_printf(NV_DBG_ERRORS, "%s%s%s%s\n",
              NVKMS_LOG_PREFIX, levelPrefix, gpuPrefix, msg);
}

/*************************************************************************
 * Per-open state. Ferrix: kernel-space (KAPI) opens only.
 *************************************************************************/

struct nvkms_per_open {
    void *data;

    enum NvKmsClientType type;

    struct NvKmsKapiDevice *device;
};

static void nvkms_kapi_event_kthread_q_callback(void *arg)
{
    struct NvKmsKapiDevice *device = arg;

    nvKmsKapiHandleEventQueueChange(device);
}

void
nvkms_event_queue_changed(nvkms_per_open_handle_t *pOpenKernel,
                          NvBool eventsAvailable)
{
    struct nvkms_per_open *popen = pOpenKernel;

    switch (popen->type) {
        case NVKMS_CLIENT_USER_SPACE:
            /* Ferrix: no user-space opens to wake. */
            break;
        case NVKMS_CLIENT_KERNEL_SPACE:
            if (eventsAvailable) {
                nvkms_queue_work(nvkms_kapi_event_kthread_q_callback,
                                 popen->device);
            }

            break;
    }
}

static void nvkms_suspend(NvU32 gpuId)
{
    nvKmsKapiSuspendResume(NV_TRUE /* suspend */);

    nvos_sema_down(&nvkms_lock);
    nvKmsSuspend(gpuId);
    nvos_sema_up(&nvkms_lock);
}

static void nvkms_resume(NvU32 gpuId)
{
    nvos_sema_down(&nvkms_lock);
    nvKmsResume(gpuId);
    nvos_sema_up(&nvkms_lock);

    nvKmsKapiSuspendResume(NV_FALSE /* suspend */);
}

static void nvkms_remove(NvU32 gpuId)
{
    nvKmsKapiRemove(gpuId);
}

static void nvkms_probe(const nv_gpu_info_t *gpu_info)
{
    nvKmsKapiProbe(gpu_info);
}

/*************************************************************************
 * Interface with resman.
 *************************************************************************/

static nvidia_modeset_rm_ops_t __rm_ops = { 0 };
static nvidia_modeset_callbacks_t nvkms_rm_callbacks = {
    .suspend = nvkms_suspend,
    .resume  = nvkms_resume,
    .remove  = nvkms_remove,
    .probe   = nvkms_probe,
};

static int nvkms_alloc_rm(void)
{
    NV_STATUS nvstatus;
    int ret;

    __rm_ops.version_string = NV_VERSION_STRING;

    nvstatus = nvidia_get_rm_ops(&__rm_ops);

    if (nvstatus != NV_OK) {
        nv_printf(NV_DBG_ERRORS, NVKMS_LOG_PREFIX "Version mismatch: "
                  "nvidia.ko(%s) nvidia-modeset.ko(%s)\n",
                  __rm_ops.version_string, NV_VERSION_STRING);
        return -EINVAL;
    }

    ret = __rm_ops.set_callbacks(&nvkms_rm_callbacks);
    if (ret < 0) {
        nv_printf(NV_DBG_ERRORS, NVKMS_LOG_PREFIX "Failed to register callbacks\n");
        return ret;
    }

    return 0;
}

void nvkms_call_rm(void *ops)
{
    nvidia_modeset_stack_ptr stack = NULL;

    if (__rm_ops.alloc_stack(&stack) != 0) {
        return;
    }

    __rm_ops.op(stack, ops);

    __rm_ops.free_stack(stack);
}

/*************************************************************************
 * ref_ptr implementation.
 *************************************************************************/

struct nvkms_ref_ptr {
    NvU64 refcnt;
    // Access to ptr is guarded by the nvkms_lock.
    void *ptr;
};

struct nvkms_ref_ptr* nvkms_alloc_ref_ptr(void *ptr)
{
    struct nvkms_ref_ptr *ref_ptr = nvkms_alloc(sizeof(*ref_ptr), NV_FALSE);
    if (ref_ptr) {
        // The ref_ptr owner counts as a reference on the ref_ptr itself.
        atomic64_set(&ref_ptr->refcnt, 1);
        ref_ptr->ptr = ptr;
    }
    return ref_ptr;
}

void nvkms_free_ref_ptr(struct nvkms_ref_ptr *ref_ptr)
{
    if (ref_ptr) {
        ref_ptr->ptr = NULL;
        // Release the owner's reference of the ref_ptr.
        nvkms_dec_ref(ref_ptr);
    }
}

void nvkms_inc_ref(struct nvkms_ref_ptr *ref_ptr)
{
    atomic64_inc(&ref_ptr->refcnt);
}

void* nvkms_dec_ref(struct nvkms_ref_ptr *ref_ptr)
{
    void *ptr = ref_ptr->ptr;
    if (atomic64_dec_and_test(&ref_ptr->refcnt)) {
        nvkms_free(ref_ptr, sizeof(*ref_ptr));
    }
    return ptr;
}

/*************************************************************************
 * Timer support
 *
 * Core NVKMS needs to be able to schedule work to execute in the
 * future, within process context.
 *
 * Ferrix: an nvos_timer fires on the timer thread, in place of Linux's
 * timer_list soft-IRQ, and from there queues nvkms_kthread_q_callback(),
 * which runs on the kthread queue.
 *************************************************************************/

struct nvkms_timer_t {
    struct nvos_timer *kernel_timer;
    NvBool cancel;
    NvBool complete;
    NvBool isRefPtr;
    nvkms_timer_proc_t *proc;
    void *dataPtr;
    NvU32 dataU32;
};

static void nvkms_timer_release(struct nvkms_timer_t *timer)
{
    if (timer->kernel_timer != NULL) {
        nvos_timer_destroy(timer->kernel_timer);
    }
    nvkms_free(timer, sizeof(*timer));
}

static void nvkms_kthread_q_callback(void *arg)
{
    struct nvkms_timer_t *timer = arg;
    void *dataPtr;

    nvos_sema_down(&nvkms_lock);

    if (timer->isRefPtr) {
        // If the object this timer refers to was destroyed, treat the timer as
        // canceled.
        dataPtr = nvkms_dec_ref(timer->dataPtr);
        if (!dataPtr) {
            timer->cancel = NV_TRUE;
        }
    } else {
        dataPtr = timer->dataPtr;
    }

    if (!timer->cancel) {
        timer->proc(dataPtr, timer->dataU32);
        timer->complete = NV_TRUE;
    }

    if (timer->isRefPtr || timer->cancel) {
        nvkms_timer_release(timer);
    }

    nvos_sema_up(&nvkms_lock);
}

static void nvkms_timer_callback(void *arg)
{
    /* On the timer thread, so queue nvkms_kthread_q_callback(). */
    nvkms_queue_work(nvkms_kthread_q_callback, arg);
}

static NvBool
nvkms_init_timer(struct nvkms_timer_t *timer, nvkms_timer_proc_t *proc,
                 void *dataPtr, NvU32 dataU32, NvBool isRefPtr, NvU64 usec)
{
    memset(timer, 0, sizeof(*timer));
    timer->cancel = NV_FALSE;
    timer->complete = NV_FALSE;
    timer->isRefPtr = isRefPtr;

    timer->proc = proc;
    timer->dataPtr = dataPtr;
    timer->dataU32 = dataU32;

    if (usec == 0) {
        nvkms_queue_work(nvkms_kthread_q_callback, timer);
    } else {
        timer->kernel_timer = nvos_timer_create(nvkms_timer_callback, timer);
        if (timer->kernel_timer == NULL) {
            return NV_FALSE;
        }
        nvos_timer_start(timer->kernel_timer, usec * 1000);
    }
    return NV_TRUE;
}

nvkms_timer_handle_t*
nvkms_alloc_timer(nvkms_timer_proc_t *proc,
                  void *dataPtr, NvU32 dataU32,
                  NvU64 usec)
{
    struct nvkms_timer_t *timer = nvkms_alloc(sizeof(*timer), NV_FALSE);
    if (timer && !nvkms_init_timer(timer, proc, dataPtr, dataU32, NV_FALSE, usec)) {
        nvkms_free(timer, sizeof(*timer));
        timer = NULL;
    }
    return timer;
}

NvBool
nvkms_alloc_timer_with_ref_ptr(nvkms_timer_proc_t *proc,
                               struct nvkms_ref_ptr *ref_ptr,
                               NvU32 dataU32, NvU64 usec)
{
    struct nvkms_timer_t *timer = nvkms_alloc(sizeof(*timer), NV_FALSE);
    if (timer) {
        // Reference the ref_ptr to make sure that it doesn't get freed before
        // the timer fires.
        nvkms_inc_ref(ref_ptr);
        if (!nvkms_init_timer(timer, proc, ref_ptr, dataU32, NV_TRUE, usec)) {
            (void)nvkms_dec_ref(ref_ptr);
            nvkms_free(timer, sizeof(*timer));
            timer = NULL;
        }
    }

    return timer != NULL;
}

void nvkms_free_timer(nvkms_timer_handle_t *handle)
{
    struct nvkms_timer_t *timer = handle;

    if (timer == NULL) {
        return;
    }

    if (timer->complete) {
        nvkms_timer_release(timer);
        return;
    }

    timer->cancel = NV_TRUE;
}

/* Ferrix: no user-space opens, so no fd is ever one of NVIDIA's files here. */
NvBool nvkms_fd_is_nvidia_chardev(int fd)
{
    return NV_FALSE;
}

void* nvkms_get_per_open_data(int fd)
{
    return NULL;
}

NvBool nvkms_open_gpu(NvU32 gpuId)
{
    nvidia_modeset_stack_ptr stack = NULL;
    NvBool ret;

    if (__rm_ops.alloc_stack(&stack) != 0) {
        return NV_FALSE;
    }

    ret = __rm_ops.open_gpu(gpuId, stack) == 0;

    __rm_ops.free_stack(stack);

    return ret;
}

void nvkms_close_gpu(NvU32 gpuId)
{
    nvidia_modeset_stack_ptr stack = NULL;

    if (__rm_ops.alloc_stack(&stack) != 0) {
        return;
    }

    __rm_ops.close_gpu(gpuId, stack);

    __rm_ops.free_stack(stack);
}

NvU32 nvkms_enumerate_gpus(nv_gpu_info_t *gpu_info)
{
    return __rm_ops.enumerate_gpus(gpu_info);
}

NvBool nvkms_allow_write_combining(void)
{
    return __rm_ops.system_info.allow_write_combining;
}

/* Ferrix: no backlight class; NVKMS treats NULL as no backlight device. */
struct nvkms_backlight_device*
nvkms_register_backlight(NvU32 gpu_id, NvU32 display_id, void *drv_priv,
                         NvU32 current_brightness)
{
    return NULL;
}

void nvkms_unregister_backlight(struct nvkms_backlight_device *nvkms_bd)
{
}

/*************************************************************************
 * NVKMS interface for kernel space NVKMS clients like KAPI
 *************************************************************************/

struct nvkms_per_open* nvkms_open_from_kapi
(
    struct NvKmsKapiDevice *device
)
{
    struct nvkms_per_open *popen = nvkms_alloc(sizeof(*popen), NV_TRUE);

    if (popen == NULL) {
        return NULL;
    }

    popen->type = NVKMS_CLIENT_KERNEL_SPACE;
    popen->device = device;

    nvos_sema_down(&nvkms_lock);
    popen->data = nvKmsOpen(os_get_current_process(),
                            NVKMS_CLIENT_KERNEL_SPACE, popen);
    nvos_sema_up(&nvkms_lock);

    if (popen->data == NULL) {
        nvkms_free(popen, sizeof(*popen));
        return NULL;
    }

    return popen;
}

void nvkms_close_from_kapi(struct nvkms_per_open *popen)
{
    nvos_sema_down(&nvkms_lock);

    nvKmsClose(popen->data);

    popen->data = NULL;

    nvos_sema_up(&nvkms_lock);

    /*
     * Flush any outstanding nvkms_kapi_event_kthread_q_callback() work
     * items before freeing popen; after nvKmsClose() no more are queued.
     */
    nvos_work_queue_flush(nvkms_kthread_q);

    nvkms_free(popen, sizeof(*popen));
}

static NvBool nvkms_ioctl_common
(
    struct nvkms_per_open *popen,
    NvU32 cmd, NvU64 address, const size_t size
)
{
    NvBool ret;

    nvos_sema_down(&nvkms_lock);

    if (popen->data != NULL) {
        ret = nvKmsIoctl(popen->data, cmd, address, size);
    } else {
        ret = NV_FALSE;
    }

    nvos_sema_up(&nvkms_lock);

    return ret;
}

NvBool nvkms_ioctl_from_kapi_try_pmlock
(
    struct nvkms_per_open *popen,
    NvU32 cmd, void *params_address, const size_t param_size
)
{
    /* Ferrix: no suspend, so the PM lock is never held against this. */
    return nvkms_ioctl_common(popen, cmd,
                              (NvU64)(NvUPtr)params_address, param_size);
}

NvBool nvkms_ioctl_from_kapi
(
    struct nvkms_per_open *popen,
    NvU32 cmd, void *params_address, const size_t param_size
)
{
    return nvkms_ioctl_common(popen, cmd,
                              (NvU64)(NvUPtr)params_address, param_size);
}

/*************************************************************************
 * APIs for locking.
 *************************************************************************/

struct nvkms_sema_t {
    nvos_sema_t os_sema;
};

nvkms_sema_handle_t* nvkms_sema_alloc(void)
{
    nvkms_sema_handle_t *sema = nvkms_alloc(sizeof(*sema), NV_TRUE);

    if (sema != NULL) {
        nvos_sema_init(&sema->os_sema, 1);
    }

    return sema;
}

void nvkms_sema_free(nvkms_sema_handle_t *sema)
{
    nvkms_free(sema, sizeof(*sema));
}

void nvkms_sema_down(nvkms_sema_handle_t *sema)
{
    nvos_sema_down(&sema->os_sema);
}

void nvkms_sema_up(nvkms_sema_handle_t *sema)
{
    nvos_sema_up(&sema->os_sema);
}

/*************************************************************************
 * Module start: nvkms_init, less the character device, procfs and the
 * config file.
 *************************************************************************/

int nvrm_kms_init(void)
{
    int ret;

    ret = nvkms_alloc_rm();

    if (ret != 0) {
        return ret;
    }

    nvos_sema_init(&nvkms_lock, 1);

    nvkms_kthread_q = nvos_work_queue_create();
    if (nvkms_kthread_q == NULL) {
        __rm_ops.set_callbacks(NULL);
        return -ENOMEM;
    }

    nvos_sema_down(&nvkms_lock);
    if (!nvKmsModuleLoad()) {
        ret = -ENOMEM;
    }
    nvos_sema_up(&nvkms_lock);

    if (ret != 0) {
        __rm_ops.set_callbacks(NULL);
    }

    return ret;
}
