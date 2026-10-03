/*
 * RM's DMA mapping calls (NVIDIA's kernel-open/nvidia/nv-dma.c), for a GPU
 * behind the IOMMU (docs/NVIDIA.md §4.3).
 *
 * ferrix-nvos pins system memory into the card's domain as it allocates it
 * (os/nvos/src/pages.rs), and the domain maps each page at its own physical
 * address. So the "physical" addresses nv_alloc_pages gave RM are already
 * the card's DMA addresses, and mapping an allocation for the device
 * changes none of them: nv_dma_map_alloc leaves va_array as it is and hands
 * back a token for unmap to free. Peer and MMIO mappings, which need a
 * second device or a BAR in the card's domain, are not supported, and RM
 * falls back as it does on a platform without them. The processors' caches
 * are coherent with PCI on x86-64, so invalidating them is nothing.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"

/* What nv_dma_map_alloc hands back as *priv, for nv_dma_unmap_alloc. */
struct nvos_dma_map {
    NvU64 page_count;
};

NV_STATUS NV_API_CALL nv_dma_map_alloc(nv_dma_device_t *dma_dev, NvU64 page_count,
                                       NvU64 *va_array, NvBool contig,
                                       NvBool read_only, void **priv)
{
    struct nvos_dma_map *map = NULL;

    (void)dma_dev;
    (void)va_array;
    (void)contig;
    (void)read_only;
    if (priv == NULL)
        return NV_ERR_INVALID_ARGUMENT;
    if (os_alloc_mem((void **)&map, sizeof(*map)) != NV_OK)
        return NV_ERR_NO_MEMORY;
    map->page_count = page_count;
    *priv = map;
    return NV_OK;
}

NV_STATUS NV_API_CALL nv_dma_unmap_alloc(nv_dma_device_t *dma_dev, NvU64 page_count,
                                         NvU64 *va_array, void **priv)
{
    (void)dma_dev;
    (void)page_count;
    (void)va_array;
    if (priv == NULL || *priv == NULL)
        return NV_ERR_NOT_SUPPORTED;
    os_free_mem(*priv);
    *priv = NULL;
    return NV_OK;
}

void NV_API_CALL nv_dma_cache_invalidate(nv_dma_device_t *dma_dev, void *priv)
{
    (void)dma_dev;
    (void)priv;
}

NV_STATUS NV_API_CALL nv_dma_map_mmio(nv_dma_device_t *dma_dev, NvU64 page_count,
                                      NvU64 *va)
{
    (void)dma_dev;
    (void)page_count;
    (void)va;
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_dma_unmap_mmio(nv_dma_device_t *dma_dev, NvU64 page_count, NvU64 va)
{
    (void)dma_dev;
    (void)page_count;
    (void)va;
}

NV_STATUS NV_API_CALL nv_dma_map_peer(nv_dma_device_t *dma_dev, nv_dma_device_t *peer,
                                      NvU8 bar_index, NvU64 page_count, NvU64 *va)
{
    (void)dma_dev;
    (void)peer;
    (void)bar_index;
    (void)page_count;
    (void)va;
    return NV_ERR_NOT_SUPPORTED;
}

void NV_API_CALL nv_dma_unmap_peer(nv_dma_device_t *dma_dev, NvU64 page_count, NvU64 va)
{
    (void)dma_dev;
    (void)page_count;
    (void)va;
}

NvBool NV_API_CALL nv_grdma_pci_topology_supported(nv_state_t *nv, nv_dma_device_t *dma_dev)
{
    (void)nv;
    (void)dma_dev;
    return NV_FALSE;
}
