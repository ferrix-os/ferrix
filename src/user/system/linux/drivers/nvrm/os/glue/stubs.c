/*
 * The imports of nv-kernel.o that nvrm does not provide yet: each fails
 * loudly, with one line naming it and why, every time it is called, and
 * answers what an absent facility answers (NV_ERR_NOT_SUPPORTED, NULL,
 * false). None is reached by RM's no-GPU initialisation; each group says
 * which step replaces it.
 *
 * Written from NVIDIA's prototypes (kernel-open/common/inc/nv.h and
 * os-interface.h) by ~/.local/share/ferrix/nvidia/n1c-wip/gen.py.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"

void nvos_stub_called(const char *name, const char *why)
{
    nvrm_say("%s called, which is a stub: %s\n", name, why);
}

/* ACPI methods: the GPU's _DSM, _ROM and friends, from the platform's tables (N1d). */

NV_STATUS NV_API_CALL nv_acpi_d3cold_dsm_for_upstream_port(nv_state_t *a0, NvU8 *a1, NvU32 a2, NvU32 a3, NvU32 *a4)
{
    nvos_stub_called("nv_acpi_d3cold_dsm_for_upstream_port", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_acpi_ddc_method(nv_state_t *a0, void *a1, NvU32 *a2, NvBool a3)
{
    nvos_stub_called("nv_acpi_ddc_method", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_acpi_dod_method(nv_state_t *a0, NvU32 *a1, NvU32 *a2)
{
    nvos_stub_called("nv_acpi_dod_method", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_acpi_dsm_method(nv_state_t *a0, NvU8 *a1, NvU32 a2, NvBool a3, NvU32 a4, void *a5, NvU16 a6, NvU32 *a7, void *a8, NvU16 *a9)
{
    nvos_stub_called("nv_acpi_dsm_method", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_acpi_get_powersource(NvU32 *a0)
{
    nvos_stub_called("nv_acpi_get_powersource", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}

NvBool NV_API_CALL nv_acpi_is_battery_present(void)
{
    nvos_stub_called("nv_acpi_is_battery_present", "no ACPI methods yet (N1d)");
    return NV_FALSE;
}

void NV_API_CALL nv_acpi_methods_init(NvU32 *a0)
{
    nvos_stub_called("nv_acpi_methods_init", "no ACPI methods yet (N1d)");
}

void NV_API_CALL nv_acpi_methods_uninit(void)
{
    nvos_stub_called("nv_acpi_methods_uninit", "no ACPI methods yet (N1d)");
}

NV_STATUS NV_API_CALL nv_acpi_mux_method(nv_state_t *a0, NvU32 *a1, NvU32 a2, const char *a3)
{
    nvos_stub_called("nv_acpi_mux_method", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_acpi_rom_method(nv_state_t *a0, NvU32 *a1, NvU32 *a2)
{
    nvos_stub_called("nv_acpi_rom_method", "no ACPI methods yet (N1d)");
    return NV_ERR_NOT_SUPPORTED;
}


/* Tegra SoC calls: clocks, BPMP, DCE, ISO bandwidth. A PCI GPU never makes them. */

NV_STATUS NV_API_CALL nv_bpmp_send_mrq(nv_state_t *a0, NvU32 a1, const void *a2, NvU32 a3, void *a4, NvU32 a5, NvS32 *a6, NvS32 *a7)
{
    nvos_stub_called("nv_bpmp_send_mrq", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_control_soc_irqs(nv_state_t *a0, NvBool a1)
{
    nvos_stub_called("nv_control_soc_irqs", "a Tegra SoC call, never made for a PCI GPU");
}

void NV_API_CALL nv_disable_clk(nv_state_t *a0, TEGRASOC_WHICH_CLK a1)
{
    nvos_stub_called("nv_disable_clk", "a Tegra SoC call, never made for a PCI GPU");
}

NV_STATUS NV_API_CALL nv_enable_clk(nv_state_t *a0, TEGRASOC_WHICH_CLK a1)
{
    nvos_stub_called("nv_enable_clk", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_get_current_irq_priv_data(nv_state_t *a0, NvU32 *a1)
{
    nvos_stub_called("nv_get_current_irq_priv_data", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_get_disp_smmu_stream_ids(nv_state_t *a0, NvU32 *a1, NvU32 *a2)
{
    nvos_stub_called("nv_get_disp_smmu_stream_ids", "a Tegra SoC call, never made for a PCI GPU");
}

NV_STATUS NV_API_CALL nv_get_max_freq(nv_state_t *a0, TEGRASOC_WHICH_CLK a1, NvU32 *a2)
{
    nvos_stub_called("nv_get_max_freq", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_get_num_dpaux_instances(nv_state_t *a0, NvU32 *a1)
{
    nvos_stub_called("nv_get_num_dpaux_instances", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_get_syncpoint_aperture(NvU32 a0, NvU64 *a1, NvU64 *a2, NvU32 *a3)
{
    nvos_stub_called("nv_get_syncpoint_aperture", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_get_tegra_brightness_level(nv_state_t *a0, NvU32 *a1)
{
    nvos_stub_called("nv_get_tegra_brightness_level", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_imp_enable_disable_rfl(nv_state_t *a0, NvBool a1)
{
    nvos_stub_called("nv_imp_enable_disable_rfl", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_imp_get_import_data(TEGRA_IMP_IMPORT_DATA *a0)
{
    nvos_stub_called("nv_imp_get_import_data", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_imp_icc_set_bw(nv_state_t *a0, NvU32 a1, NvU32 a2)
{
    nvos_stub_called("nv_imp_icc_set_bw", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_pci_tegra_pm_deinit(nv_state_t *a0)
{
    nvos_stub_called("nv_pci_tegra_pm_deinit", "a Tegra SoC call, never made for a PCI GPU");
}

NvBool NV_API_CALL nv_pci_tegra_pm_init(nv_state_t *a0)
{
    nvos_stub_called("nv_pci_tegra_pm_init", "a Tegra SoC call, never made for a PCI GPU");
    return NV_FALSE;
}

NV_STATUS NV_API_CALL nv_set_freq(nv_state_t *a0, TEGRASOC_WHICH_CLK a1, NvU32 a2)
{
    nvos_stub_called("nv_set_freq", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_set_tegra_brightness_level(nv_state_t *a0, NvU32 a1)
{
    nvos_stub_called("nv_set_tegra_brightness_level", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_tegra_dce_client_ipc_send_recv(NvU32 a0, void *a1, NvU32 a2)
{
    nvos_stub_called("nv_tegra_dce_client_ipc_send_recv", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_tegra_dce_register_ipc_client(NvU32 a0, void *a1, nvTegraDceClientIpcCallback a2, NvU32 *a3)
{
    nvos_stub_called("nv_tegra_dce_register_ipc_client", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_tegra_dce_unregister_ipc_client(NvU32 a0)
{
    nvos_stub_called("nv_tegra_dce_unregister_ipc_client", "a Tegra SoC call, never made for a PCI GPU");
    return NV_ERR_NOT_SUPPORTED;
}

NvU32 NV_API_CALL nv_tegra_get_rm_interface_type(NvU32 a0)
{
    nvos_stub_called("nv_tegra_get_rm_interface_type", "a Tegra SoC call, never made for a PCI GPU");
    return (NvU32)~0;
}


/* DMA mapping of system memory and peers through the IOMMU (N1d). */

void NV_API_CALL nv_dma_cache_invalidate(nv_dma_device_t *a0, void *a1)
{
    nvos_stub_called("nv_dma_cache_invalidate", "DMA mapping comes with N1d");
}

void* NV_API_CALL nv_dma_get_dev_pagemap(NvU64 a0)
{
    nvos_stub_called("nv_dma_get_dev_pagemap", "DMA mapping comes with N1d");
    return NULL;
}

NV_STATUS NV_API_CALL nv_dma_import_dma_buf(nv_dma_device_t *a0, struct dma_buf *a1, NvBool a2, NvU32 *a3, struct sg_table **a4, nv_dma_buf_t **a5)
{
    nvos_stub_called("nv_dma_import_dma_buf", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_dma_import_from_fd(nv_dma_device_t *a0, NvS32 a1, NvBool a2, NvU32 *a3, struct sg_table **a4, nv_dma_buf_t **a5)
{
    nvos_stub_called("nv_dma_import_from_fd", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_dma_import_sgt(nv_dma_device_t *a0, struct sg_table *a1, struct drm_gem_object *a2)
{
    nvos_stub_called("nv_dma_import_sgt", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_dma_map_alloc(nv_dma_device_t *a0, NvU64 a1, NvU64 *a2, NvBool a3, NvBool a4, void **a5)
{
    nvos_stub_called("nv_dma_map_alloc", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_dma_map_mmio(nv_dma_device_t *a0, NvU64 a1, NvU64 *a2)
{
    nvos_stub_called("nv_dma_map_mmio", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL nv_dma_map_peer(nv_dma_device_t *a0, nv_dma_device_t *a1, NvU8 a2, NvU64 a3, NvU64 *a4)
{
    nvos_stub_called("nv_dma_map_peer", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_dma_put_dev_pagemap(void *a0)
{
    nvos_stub_called("nv_dma_put_dev_pagemap", "DMA mapping comes with N1d");
}

void NV_API_CALL nv_dma_release_dma_buf(nv_dma_buf_t *a0)
{
    nvos_stub_called("nv_dma_release_dma_buf", "DMA mapping comes with N1d");
}

void NV_API_CALL nv_dma_release_sgt(struct sg_table *a0, struct drm_gem_object *a1)
{
    nvos_stub_called("nv_dma_release_sgt", "DMA mapping comes with N1d");
}

NV_STATUS NV_API_CALL nv_dma_unmap_alloc(nv_dma_device_t *a0, NvU64 a1, NvU64 *a2, void **a3)
{
    nvos_stub_called("nv_dma_unmap_alloc", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_dma_unmap_mmio(nv_dma_device_t *a0, NvU64 a1, NvU64 a2)
{
    nvos_stub_called("nv_dma_unmap_mmio", "DMA mapping comes with N1d");
}

void NV_API_CALL nv_dma_unmap_peer(nv_dma_device_t *a0, NvU64 a1, NvU64 a2)
{
    nvos_stub_called("nv_dma_unmap_peer", "DMA mapping comes with N1d");
}

NV_STATUS NV_API_CALL nv_get_phys_pages(void *a0, void *a1, NvU32 *a2)
{
    nvos_stub_called("nv_get_phys_pages", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

NvBool NV_API_CALL nv_grdma_pci_topology_supported(nv_state_t *a0, nv_dma_device_t *a1)
{
    nvos_stub_called("nv_grdma_pci_topology_supported", "DMA mapping comes with N1d");
    return NV_FALSE;
}

NV_STATUS NV_API_CALL nv_register_sgt(nv_state_t *a0, NvU64 *a1, NvU64 a2, NvU32 a3, void **a4, struct sg_table *a5, void *a6, NvBool a7)
{
    nvos_stub_called("nv_register_sgt", "DMA mapping comes with N1d");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_unregister_sgt(nv_state_t *a0, struct sg_table **a1, void **a2, void *a3)
{
    nvos_stub_called("nv_unregister_sgt", "DMA mapping comes with N1d");
}


/* The display's i2c buses (N6). */

void* NV_API_CALL nv_i2c_add_adapter(nv_state_t *a0, NvU32 a1)
{
    nvos_stub_called("nv_i2c_add_adapter", "the display's i2c comes with N6");
    return NULL;
}

NV_STATUS NV_API_CALL nv_i2c_bus_status(nv_state_t *a0, NvU32 a1, NvS32 *a2, NvS32 *a3)
{
    nvos_stub_called("nv_i2c_bus_status", "the display's i2c comes with N6");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_i2c_del_adapter(nv_state_t *a0, void *a1)
{
    nvos_stub_called("nv_i2c_del_adapter", "the display's i2c comes with N6");
}

NV_STATUS NV_API_CALL nv_i2c_transfer(nv_state_t *a0, NvU32 a1, NvU8 a2, nv_i2c_msg_t *a3, int a4)
{
    nvos_stub_called("nv_i2c_transfer", "the display's i2c comes with N6");
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_i2c_unregister_clients(nv_state_t *a0)
{
    nvos_stub_called("nv_i2c_unregister_clients", "the display's i2c comes with N6");
}


/* x86 port I/O, which no Ferrix driver is given. */

NvU8 NV_API_CALL os_io_read_byte(NvU32 a0)
{
    nvos_stub_called("os_io_read_byte", "port I/O is never given to a driver");
    return (NvU8)~0;
}

NvU32 NV_API_CALL os_io_read_dword(NvU32 a0)
{
    nvos_stub_called("os_io_read_dword", "port I/O is never given to a driver");
    return (NvU32)~0;
}

NvU16 NV_API_CALL os_io_read_word(NvU32 a0)
{
    nvos_stub_called("os_io_read_word", "port I/O is never given to a driver");
    return (NvU16)~0;
}

void NV_API_CALL os_io_write_byte(NvU32 a0, NvU8 a1)
{
    nvos_stub_called("os_io_write_byte", "port I/O is never given to a driver");
}

void NV_API_CALL os_io_write_dword(NvU32 a0, NvU32 a1)
{
    nvos_stub_called("os_io_write_dword", "port I/O is never given to a driver");
}

void NV_API_CALL os_io_write_word(NvU32 a0, NvU16 a1)
{
    nvos_stub_called("os_io_write_word", "port I/O is never given to a driver");
}


/* A client's own pages, pinned or looked up (N1e). */

NvU32 NV_API_CALL os_count_tail_pages(NvU64 a0)
{
    nvos_stub_called("os_count_tail_pages", "client pages come with N1e");
    return (NvU32)~0;
}

NV_STATUS NV_API_CALL os_get_page(NvU64 a0)
{
    nvos_stub_called("os_get_page", "client pages come with N1e");
    return NV_ERR_NOT_SUPPORTED;
}

NvU32 NV_API_CALL os_get_page_refcount(NvU64 a0)
{
    nvos_stub_called("os_get_page_refcount", "client pages come with N1e");
    return (NvU32)~0;
}

NV_STATUS NV_API_CALL os_lock_user_pages(void *a0, NvU64 a1, void **a2, NvU32 a3)
{
    nvos_stub_called("os_lock_user_pages", "client pages come with N1e");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL os_lookup_user_io_memory(void *a0, NvU64 a1, NvU64 **a2)
{
    nvos_stub_called("os_lookup_user_io_memory", "client pages come with N1e");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL os_put_page(NvU64 a0)
{
    nvos_stub_called("os_put_page", "client pages come with N1e");
    return NV_ERR_NOT_SUPPORTED;
}

NV_STATUS NV_API_CALL os_unlock_user_pages(NvU64 a0, void *a1, NvU32 a2)
{
    nvos_stub_called("os_unlock_user_pages", "client pages come with N1e");
    return NV_ERR_NOT_SUPPORTED;
}


/* Fabric management, surprise removal and the GPU-gone check. */

NV_STATUS NV_API_CALL nv_acquire_fabric_mgmt_cap(int a0, int*a1)
{
    nvos_stub_called("nv_acquire_fabric_mgmt_cap", "not on Ferrix yet");
    return NV_ERR_NOT_SUPPORTED;
}

NvBool NV_API_CALL nv_is_gpu_accessible(nv_state_t *a0)
{
    nvos_stub_called("nv_is_gpu_accessible", "not on Ferrix yet");
    return NV_FALSE;
}

void NV_API_CALL os_pci_remove(void *a0)
{
    nvos_stub_called("os_pci_remove", "not on Ferrix yet");
}
