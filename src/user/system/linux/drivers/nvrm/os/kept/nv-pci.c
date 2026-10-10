/*
 * SPDX-FileCopyrightText: Copyright (c) 2019-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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
 * Kept for Ferrix (docs/NVIDIA.md §4.1, §7's N1d) from NVIDIA's
 * kernel-open/nvidia/nv-pci.c (nv_pci_probe) and nv.c (nv_start_device,
 * nvidia_isr, nvidia_isr_common_bh) at 580.173.02, under the MIT licence
 * above.
 *
 * What is kept: the probe's steps that set RM's view of the GPU up -- the
 * legacy check, BAR0 first, the state's identity, the locks,
 * rm_is_supported_device and rm_init_private_state, the other BARs, bus
 * mastering's flag, the firmware request and the device list -- and the
 * start's interrupt and rm_init_adapter, which boots the GSP. Each step
 * keeps NVIDIA's order and message.
 *
 * What changed: the PCI device is the one devmgr handed nvrm, which
 * ferrix-nvos attached (os/nvos/src/device.rs), so its BARs are its
 * apertures and its configuration space is N0d's window. Enabling the
 * device, its memory decoding and bus mastering are the kernel's (the
 * first pin turns bus mastering on). The interrupt is the device's one MSI
 * vector, claimed through nvos_interrupt_start, whose thread runs the top
 * half and, when RM asks for it, the bottom half: there is no other
 * thread on the interrupt's path. Not kept: SR-IOV, ATS and SVA, Tegra
 * and self-hosted GPUs, NUMA, resizable BARs (QEMU passes one size), VGA
 * arbitration, dynamic power management, procfs, vGPU and UVM's notices.
 *
 * nvrm_gpu_stop (docs/NVIDIA.md §13) is nv.c's nv_shutdown_adapter for the
 * GPU nvrm_gpu_start started: rm_disable_adapter, the interrupt's end,
 * rm_shutdown_adapter, in NVIDIA's order. What changed: Linux ends the
 * interrupt with free_irq, which waits for a running handler; nvos has no
 * call that gives the vector back, so the handler is told to do nothing
 * from then on and the one that may be running is waited for.
 */

#include "nv-ferrix.h"

/* NVIDIA's NV_PCI_DEVFN macros, over nvos's location word. */
#define LOCATION_DOMAIN(l)  (((l) >> 16) & 0xffff)
#define LOCATION_BUS(l)     (((l) >> 8) & 0xff)
#define LOCATION_SLOT(l)    (((l) >> 3) & 0x1f)
#define LOCATION_FUNC(l)    ((l) & 0x7)

/* The configuration space's subsystem identifiers. */
#define PCI_SUBSYSTEM_VENDOR_ID 0x2c
#define PCI_SUBSYSTEM_ID        0x2e

/* Where nvrm_gpu_start gave up. */
enum gpu_step {
    GPU_STEP_DESCRIBE = 20,
    GPU_STEP_LEGACY = 21,
    GPU_STEP_BAR0 = 22,
    GPU_STEP_MEMORY = 23,
    GPU_STEP_LOCKS = 24,
    GPU_STEP_SUPPORTED = 25,
    GPU_STEP_PRIVATE = 26,
    GPU_STEP_INTERRUPT = 27,
    GPU_STEP_ADAPTER = 28,
};

/*
 * NVIDIA's nvidia_isr and nvidia_isr_common_bh, as one handler for the
 * interrupt thread: MMU faults first, then rm_isr, and its bottom half at
 * once when it asks for one. Linux runs the bottom half on a kthread
 * after IRQ_WAKE_THREAD; here the interrupt thread is already a thread
 * that may sleep, and its acknowledgement waits until both halves ran.
 */
/* The GPU nvrm_gpu_start started, for nvrm_gpu_stop; whether its interrupt
 * has ended; and the handlers running now. */
static nv_linux_state_t *nvrm_gpu_started;
static NvU32 nvrm_gpu_isr_ended;
static NvU32 nvrm_gpu_isr_running;

static void nvrm_gpu_isr(void *argument)
{
    nv_linux_state_t *nvl = argument;
    nv_state_t *nv = NV_STATE_PTR(nvl);
    NvU32 need_bottom_half = 0;
    NvU32 faults = 0;

    /* Counted before the look, so that nvrm_gpu_stop, which sets the flag
     * and then waits for the count, never misses a handler that got in. */
    __atomic_fetch_add(&nvrm_gpu_isr_running, 1, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(&nvrm_gpu_isr_ended, __ATOMIC_SEQ_CST))
    {
        __atomic_fetch_sub(&nvrm_gpu_isr_running, 1, __ATOMIC_SEQ_CST);
        return;
    }
    nvos_isr_enter_leave(NV_TRUE);
    rm_gpu_handle_mmu_faults(nvl->sp_isr, nv, &faults);
    (void)rm_isr(nvl->sp_isr, nv, &need_bottom_half);
    nvos_isr_enter_leave(NV_FALSE);
    if (need_bottom_half || faults != 0)
        rm_isr_bh(nvl->sp_bh, nv);
    __atomic_fetch_sub(&nvrm_gpu_isr_running, 1, __ATOMIC_SEQ_CST);
}

/* Fill RM's BAR `j` from aperture `i` of `desc`. */
static void set_bar(nv_state_t *nv, NvU32 j, const struct nvos_device_desc *desc, NvU32 i)
{
    nv->bars[j].offset = NVRM_PCICFG_BAR_OFFSET(desc->bar[i]);
    nv->bars[j].cpu_address = desc->phys[i];
    nv->bars[j].size = desc->len[i];
}

int nvrm_gpu_start(void)
{
    struct nvos_device_desc desc;
    nv_linux_state_t *nvl = NULL;
    nv_state_t *nv;
    nvidia_stack_t *sp = NULL;
    void *handle;
    NvU32 i;
    NvU16 subsystem_vendor = 0, subsystem_device = 0;

    os_mem_set(&desc, 0, sizeof(desc));
    if (nvos_device_describe(&desc) != NV_OK)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: no GPU attached to probe\n");
        return GPU_STEP_DESCRIBE;
    }

    nv_printf(NV_DBG_ERRORS, "NVRM: probing 0x%x 0x%x, class 0x%x\n",
              desc.vendor, desc.device, desc.class_code);

    if (nv_kmem_cache_alloc_stack(&sp) != 0)
        return GPU_STEP_MEMORY;

    handle = os_pci_init_handle(LOCATION_DOMAIN(desc.location),
                                LOCATION_BUS(desc.location),
                                LOCATION_SLOT(desc.location),
                                LOCATION_FUNC(desc.location), NULL, NULL);
    if (handle != NULL)
    {
        os_pci_read_word(handle, PCI_SUBSYSTEM_VENDOR_ID, &subsystem_vendor);
        os_pci_read_word(handle, PCI_SUBSYSTEM_ID, &subsystem_device);
    }

    if (!rm_is_supported_pci_device((desc.class_code >> 16) & 0xff,
                                    (desc.class_code >> 8) & 0xff,
                                    desc.vendor, desc.device,
                                    subsystem_vendor, subsystem_device,
                                    NV_FALSE))
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: ignoring the legacy GPU %04x:%02x:%02x.%x\n",
                  LOCATION_DOMAIN(desc.location), LOCATION_BUS(desc.location),
                  LOCATION_SLOT(desc.location), LOCATION_FUNC(desc.location));
        return GPU_STEP_LEGACY;
    }

    /* BAR0, the registers: the first memory aperture, from BAR 0. */
    for (i = 0; i < desc.apertures; i++)
        if (desc.bar[i] == 0)
            break;
    if (i == desc.apertures || desc.len[i] == 0)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: BAR0 is not usable\n");
        return GPU_STEP_BAR0;
    }

    NV_KZALLOC(nvl, sizeof(nv_linux_state_t));
    if (nvl == NULL)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: failed to allocate memory\n");
        return GPU_STEP_MEMORY;
    }
    nv = NV_STATE_PTR(nvl);

    set_bar(nv, NV_GPU_BAR_INDEX_REGS, &desc, i);
    nv->regs = &nv->bars[NV_GPU_BAR_INDEX_REGS];

    /* Default to 32-bit PCI bus address space, as Linux's probe does. */
    nvl->dma_mask = 0xffffffffULL;
    nvl->dma_dev.addressable_range.start = 0;
    nvl->dma_dev.addressable_range.limit = 0xffffffffULL;
    nvl->device_handle = 1;

    nv->pci_info.vendor_id = desc.vendor;
    nv->pci_info.device_id = desc.device;
    nv->subsystem_id       = subsystem_device;
    nv->subsystem_vendor   = subsystem_vendor;
    nv->os_state           = (void *)nvl;
    nv->dma_dev            = &nvl->dma_dev;
    nv->pci_info.domain    = LOCATION_DOMAIN(desc.location);
    nv->pci_info.bus       = LOCATION_BUS(desc.location);
    nv->pci_info.slot      = LOCATION_SLOT(desc.location);
    nv->pci_info.function  = LOCATION_FUNC(desc.location);
    nv->handle             = handle;

    if (!nv_lock_init_locks(sp, nv))
        return GPU_STEP_LOCKS;

    if (rm_is_supported_device(sp, nv) != NV_OK)
        return GPU_STEP_SUPPORTED;

    /* Wire RM HAL */
    if (!rm_init_private_state(sp, nv))
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: rm_init_private_state() failed!\n");
        return GPU_STEP_PRIVATE;
    }

    nvl->all_mappings_revoked = NV_TRUE;
    nvl->safe_to_mmap = NV_TRUE;

    /*
     * The other memory BARs. Linux walks the PCI resources in order, and a
     * 64-bit BAR takes two, so on a GPU BAR 1 is the framebuffer and BAR 3
     * the instance memory; here an aperture names its BAR, and a BAR the
     * kernel withheld has none, so each is placed by its number.
     */
    for (i = 0; i < desc.apertures; i++)
    {
        if (desc.bar[i] == 1)
            set_bar(nv, NV_GPU_BAR_INDEX_FB, &desc, i);
        else if (desc.bar[i] == 3)
            set_bar(nv, NV_GPU_BAR_INDEX_IMEM, &desc, i);
    }
    if (nv->bars[NV_GPU_BAR_INDEX_FB].size == 0)
        nv_printf(NV_DBG_ERRORS, "NVRM: BAR1 (the framebuffer) was withheld\n");
    nv->fb = &nv->bars[NV_GPU_BAR_INDEX_FB];
    nv->interrupt_line = 0;

    nv_printf(NV_DBG_ERRORS,
              "NVRM: PCI:%04x:%02x:%02x.%x (%04x:%04x): BAR0 @ 0x%llx (%lluMB)\n",
              nv->pci_info.domain, nv->pci_info.bus, nv->pci_info.slot,
              nv->pci_info.function, nv->pci_info.vendor_id, nv->pci_info.device_id,
              nv->regs->cpu_address, (nv->regs->size >> 20));
    nv_printf(NV_DBG_ERRORS,
              "NVRM: PCI:%04x:%02x:%02x.%x (%04x:%04x): BAR1 @ 0x%llx (%lluMB)\n",
              nv->pci_info.domain, nv->pci_info.bus, nv->pci_info.slot,
              nv->pci_info.function, nv->pci_info.vendor_id, nv->pci_info.device_id,
              nv->fb->cpu_address, (nv->fb->size >> 20));

    nv_linux_add_device(nvl);
    rm_set_rm_firmware_requested(sp, nv);

    /* nv_start_device: the interrupt, then RM's adapter. */
    if (nv_kmem_cache_alloc_stack(&nvl->sp_isr) != 0 ||
        nv_kmem_cache_alloc_stack(&nvl->sp_bh) != 0)
        return GPU_STEP_MEMORY;

    if (desc.vectors == 0)
    {
        nv_printf(NV_DBG_ERRORS,
                  "NVRM: No interrupts of any type are available. Cannot use this GPU.\n");
        return GPU_STEP_INTERRUPT;
    }
    if (nvos_interrupt_start(0, nvrm_gpu_isr, nvl) != NV_OK)
    {
        nv_printf(NV_DBG_ERRORS, "NVRM: request_irq() failed\n");
        return GPU_STEP_INTERRUPT;
    }
    nv->flags |= NV_FLAG_USES_MSI;

    if (!rm_init_adapter(sp, nv))
    {
        nv_printf(NV_DBG_ERRORS,
                  "NVRM: rm_init_adapter failed, device minor number %d\n",
                  nvl->minor_num);
        return GPU_STEP_ADAPTER;
    }

    nv->flags |= NV_FLAG_OPEN;
    nvrm_gpu_started = nvl;
    nv_printf(NV_DBG_ERRORS, "NVRM: GPU %04x:%02x:%02x.%x: rm_init_adapter succeeded\n",
              nv->pci_info.domain, nv->pci_info.bus, nv->pci_info.slot,
              nv->pci_info.function);
    return 0;
}

/* How long a running interrupt handler is waited for, and how often it is
 * looked at. */
#define GPU_STOP_ISR_PATIENCE_MS 2000
#define GPU_STOP_ISR_POLL_MS     5

/*
 * Shut the started GPU's adapter down: NVIDIA's nv_shutdown_adapter.
 * rm_disable_adapter turns the GPU's interrupts off and unloads what the
 * clients left; rm_shutdown_adapter unloads and destroys the GPU's state,
 * which unloads the GSP firmware. 0, 1 when no GPU is started, 3 when
 * there was no memory for RM's stack, or 2 when an interrupt handler was
 * still running after the wait, and the adapter was left disabled but not
 * shut down, since RM would free the state the handler is in.
 */
int nvrm_gpu_stop(void)
{
    nv_linux_state_t *nvl = nvrm_gpu_started;
    nv_state_t *nv;
    nvidia_stack_t *sp = NULL;
    NvU32 waited = 0;

    if (nvl == NULL)
        return 1;
    nv = NV_STATE_PTR(nvl);
    if (nv_kmem_cache_alloc_stack(&sp) != 0)
        return 3;

    nvos_sema_down(&nvl->ldata_lock);
    rm_disable_adapter(sp, nv);

    __atomic_store_n(&nvrm_gpu_isr_ended, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(&nvrm_gpu_isr_running, __ATOMIC_SEQ_CST) != 0 &&
           waited < GPU_STOP_ISR_PATIENCE_MS)
    {
        (void)os_delay(GPU_STOP_ISR_POLL_MS);
        waited += GPU_STOP_ISR_POLL_MS;
    }
    if (__atomic_load_n(&nvrm_gpu_isr_running, __ATOMIC_SEQ_CST) != 0)
    {
        nvos_sema_up(&nvl->ldata_lock);
        nv_kmem_cache_free_stack(sp);
        return 2;
    }

    rm_shutdown_adapter(sp, nv);

    /* As nv_stop_device leaves it: opens find no GPU from here on. */
    nv->flags &= ~NV_FLAG_OPEN;
    nvrm_gpu_started = NULL;
    nvos_sema_up(&nvl->ldata_lock);
    nv_kmem_cache_free_stack(sp);
    return 0;
}
