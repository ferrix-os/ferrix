/*
 * nv-ferrix.h: the OS state NVIDIA's kept C keeps on Ferrix, in place of
 * NVIDIA's nv-linux.h (docs/NVIDIA.md §4.2).
 *
 * The kept files (os/kept/) are NVIDIA's MIT C from kernel-open/nvidia,
 * with their Linux calls replaced. On Linux they include nv-linux.h, 1,852
 * lines over most of the kernel's headers; here they include this. It keeps
 * NVIDIA's names for the per-device and per-file state -- nv_linux_state_t
 * and nv_linux_file_private_t -- so that the kept code reads as NVIDIA
 * wrote it, but every field is Ferrix's, and RM never looks inside either:
 * it sees nv_state_t (nv.h, NVIDIA's, unchanged) and its opaque os_state.
 *
 * SPDX-License-Identifier: MIT
 */
#ifndef NV_FERRIX_H
#define NV_FERRIX_H

#include <stddef.h>
#include <stdarg.h>

#include "nv.h"
#include "os-interface.h"
#include "nv-ioctl.h"
#include "nvos.h"

/* ------------------------------------------------------------------------
 * The C library calls the kept C makes. Declared here rather than from the
 * C library's headers, as uvm-kpi's kpi/libc.h does, so that one header
 * set serves both the host's glibc and ferrousli.
 * ---------------------------------------------------------------------- */
void *malloc(size_t);
void *calloc(size_t, size_t);
void free(void *);
void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
int memcmp(const void *, const void *, size_t);
size_t strlen(const char *);
int strcmp(const char *, const char *);
int strncmp(const char *, const char *, size_t);
char *strsep(char **, const char *);
int vsnprintf(char *, size_t, const char *, va_list);
int snprintf(char *, size_t, const char *, ...);

#define PAGE_SHIFT          12
#define PAGE_SIZE           (1UL << PAGE_SHIFT)
#define NV_PAGE_MASK        (~(PAGE_SIZE - 1))

#define NV_KMALLOC(ptr, size)   ((ptr) = malloc(size))
#define NV_KZALLOC(ptr, size)   ((ptr) = calloc(1, size))
#define NV_KFREE(ptr, size)     free(ptr)

#define simple_strtoul(s, e, b) os_strtoul((s), (e), (b))

#define container_of(p, type, member) \
    ((type *)((char *)(p) - offsetof(type, member)))

#define smp_load_acquire(p)         __atomic_load_n((p), __ATOMIC_ACQUIRE)
#define smp_store_release(p, v)     __atomic_store_n((p), (v), __ATOMIC_RELEASE)
#define atomic64_inc(p)             ((void)__atomic_fetch_add((p), 1, __ATOMIC_SEQ_CST))
#define atomic64_dec_and_test(p)    (__atomic_sub_fetch((p), 1, __ATOMIC_SEQ_CST) == 0)
#define atomic64_read(p)            __atomic_load_n((p), __ATOMIC_SEQ_CST)
#define atomic64_set(p, v)          __atomic_store_n((p), (v), __ATOMIC_SEQ_CST)

/* As NVIDIA's nv-linux.h: a mapping smaller than an OS page needs 4K
 * isolation only where pages are larger than RM's, which x86-64's are not. */
#define NV_4K_PAGE_ISOLATION_REQUIRED(addr, size)                       \
    ((PAGE_SIZE > NV_RM_PAGE_SIZE) &&                                   \
     ((size) <= NV_RM_PAGE_SIZE) &&                                     \
     (((addr) >> NV_RM_PAGE_SHIFT) ==                                   \
        (((addr) + (size) - 1) >> NV_RM_PAGE_SHIFT)))

/* RM's alternate stack, handed to every entry point (altstacks are off). */
static inline int nv_kmem_cache_alloc_stack(nvidia_stack_t **stack)
{
    *stack = calloc(1, sizeof(nvidia_stack_t));
    if (*stack == NULL)
        return -1;
    (*stack)->size = sizeof((*stack)->stack);
    (*stack)->top = (*stack)->stack + sizeof((*stack)->stack);
    return 0;
}

static inline void nv_kmem_cache_free_stack(nvidia_stack_t *stack)
{
    free(stack);
}

/* ------------------------------------------------------------------------
 * System memory: an allocation as RM's pAllocPrivate, over ferrix-nvos's
 * pinned pages (os/kept/nv.c).
 * ---------------------------------------------------------------------- */
typedef struct nvidia_pte_s {
    NvU64 phys_addr;
    NvUPtr virt_addr;
} nvidia_pte_t;

typedef struct nv_alloc_s {
    struct nv_alloc_s *next;
    NvU64 usage_count;
    struct {
        NvBool contig    : 1;
        NvBool guest     : 1;
        NvBool zeroed    : 1;
        NvBool aliased   : 1;
        NvBool user      : 1;
        NvBool node      : 1;
        NvBool peer_io   : 1;
        NvBool physical  : 1;
        NvBool unencrypted : 1;
        NvBool coherent  : 1;
        NvBool carveout  : 1;
    } flags;
    unsigned int cache_type;
    unsigned int num_pages;
    unsigned int pid;
    NvU64 guest_id;
    void *import_priv;
    void *user_pages;
    struct nvos_pages *pages;     /* ferrix-nvos's allocation, or NULL */
    nvidia_pte_t *page_table;
} nv_alloc_t;

/* ------------------------------------------------------------------------
 * Per device: the control device, and each GPU at N1d.
 * ---------------------------------------------------------------------- */
/* NVIDIA's nv-linux.h nv_dma_device: what RM's DMA calls are handed. Its
 * device pointer names nothing here; the range is what RM set. */
struct nv_dma_device {
    struct {
        NvU64 start;
        NvU64 limit;
    } addressable_range;

    void *dev;
};

typedef struct nv_linux_state_s {
    nv_state_t nv_state;            /* first: RM's view */

    NvU64 usage_count;
    nvos_sema_t ldata_lock;         /* NVIDIA's semaphore of one */
    nvos_sema_t mmap_lock;
    NvU32 minor_num;
    struct nv_linux_state_s *next;
    NvU64 numa_memblock_size;

    /* Mapping revocation (nv-mmap.c). */
    NvBool all_mappings_revoked;
    NvBool safe_to_mmap;

    /* The 1 Hz RC timer. */
    struct nvos_timer *rc_timer;

    /* The DMA mask RM set (nv_set_dma_address_size). */
    NvU64 dma_mask;

    /* The device handle ferrix-nvos attached, for a GPU. */
    NvU32 device_handle;

    /* For a GPU (os/kept/nv-pci.c): its DMA device, and RM's stacks for
     * the interrupt thread's top and bottom halves. */
    nv_dma_device_t dma_dev;
    nvidia_stack_t *sp_isr;
    nvidia_stack_t *sp_bh;
} nv_linux_state_t;

/* ------------------------------------------------------------------------
 * Per open file of /dev/nvidiactl or /dev/nvidiaN. N1e's forwarding core
 * names each by a file identity; nvrm-link-test by the pseudo-descriptor
 * nvrm_file_open gave it.
 * ---------------------------------------------------------------------- */
typedef struct nvidia_event {
    struct nvidia_event *next;
    nv_event_t event;
} nvidia_event_t;

typedef struct nv_linux_file_private_s {
    nv_file_private_t nvfp;         /* RM's view */

    nvidia_stack_t *sp;
    nv_alloc_t *free_list;
    nv_linux_state_t *nvptr;
    nvidia_event_t *event_data_head, *event_data_tail;
    NvBool dataless_event_pending;
    nvos_mutex_t fp_lock;           /* NVIDIA's spinlock */
    nvos_event_t waitqueue;         /* poll's wakeup, for N1e */
    NvU32 *attached_gpus;
    size_t num_attached_gpus;
    nv_alloc_mapping_context_t mmap_context;
    int open_rc;
    NV_STATUS adapter_status;

    NvS32 fd;                       /* the identity RM's fd numbers name */
    struct nv_linux_file_private_s *next_open;
} nv_linux_file_private_t;

static inline nv_linux_file_private_t *nv_get_nvlfp_from_nvfp(nv_file_private_t *nvfp)
{
    return container_of(nvfp, nv_linux_file_private_t, nvfp);
}

#define NV_GET_NVL_FROM_NV_STATE(nv)    ((nv_linux_state_t *)(nv)->os_state)
#define NV_STATE_PTR(nvl)               (&((nv_linux_state_t *)(nvl))->nv_state)
#define NV_IS_CTL_DEVICE(nv)            ((nv)->flags & NV_FLAG_CONTROL)

extern nv_linux_state_t nv_ctl_device;

/* ------------------------------------------------------------------------
 * nvrm's file interface (os/kept/nv.c): what the forwarding core of §4.4
 * calls at N1e, and what nvrm-link-test calls as a client would.
 * ---------------------------------------------------------------------- */
int nvrm_module_init(void);

/* os/kept/nv.c: a probed GPU joins the device list. */
void nv_linux_add_device(nv_linux_state_t *nvl);
NvBool nv_lock_init_locks(nvidia_stack_t *sp, nv_state_t *nv);

/* os/kept/nv-pci.c: probe the attached GPU and start it, through
 * rm_init_adapter; 0, or the step that failed. */
int nvrm_gpu_start(void);
void nvrm_module_exit(void);
nv_linux_file_private_t *nvrm_open_ctl(void);
int nvrm_ioctl(nv_linux_file_private_t *nvlfp, unsigned int cmd, void *arg);
void nvrm_close(nv_linux_file_private_t *nvlfp);

/* The ioctl encoding of Linux's asm-generic/ioctl.h, which RM's numbers use. */
#define _IOC_NRBITS     8
#define _IOC_TYPEBITS   8
#define _IOC_SIZEBITS   14
#define _IOC_NRSHIFT    0
#define _IOC_TYPESHIFT  (_IOC_NRSHIFT + _IOC_NRBITS)
#define _IOC_SIZESHIFT  (_IOC_TYPESHIFT + _IOC_TYPEBITS)
#define _IOC_DIRSHIFT   (_IOC_SIZESHIFT + _IOC_SIZEBITS)
#define _IOC_NR(nr)     (((nr) >> _IOC_NRSHIFT) & ((1 << _IOC_NRBITS) - 1))
#define _IOC_SIZE(nr)   (((nr) >> _IOC_SIZESHIFT) & ((1 << _IOC_SIZEBITS) - 1))
#define _IOC(dir, type, nr, size) \
    (((dir) << _IOC_DIRSHIFT) | ((type) << _IOC_TYPESHIFT) | \
     ((nr) << _IOC_NRSHIFT) | ((size) << _IOC_SIZESHIFT))
#define _IOWR(type, nr, size)   _IOC(3U, (type), (nr), sizeof(size))

/* errno values the dispatcher returns, as Linux's. */
#define EIO     5
#define ENOMEM  12
#define EFAULT  14
#define EBUSY   16
#define EINVAL  22
#define EACCES  13

/* The level of nv.c's memory accounting lines: nv-linux.h's without
 * NV_MEM_LOGGER, so they print only when every info line does. */
#define NV_DBG_MEMINFO  NV_DBG_INFO

/* A line for this layer's own messages. */
#define nvrm_say(...)   nv_printf(NV_DBG_ERRORS, "nvrm: " __VA_ARGS__)

/* RM's own data, which nvrm reads through the table nvos_core_load fills
 * from the core's export header (docs/NVIDIA.md §4.1, "The core"). Data
 * comes first in the table, in core/core-link.py's DATA order, so these
 * indices are fixed; a function of the core is called by its own name,
 * through a generated stub. */
extern unsigned long long nvrm_core_table[];
#define NVRM_CORE_DATA_pNVRM_ID 0
#define pNVRM_ID (*(const char *const *)nvrm_core_table[NVRM_CORE_DATA_pNVRM_ID])

/* A stub that fails loudly (glue/stubs.c): one line naming it, each time. */
void nvos_stub_called(const char *name, const char *why);

#endif
