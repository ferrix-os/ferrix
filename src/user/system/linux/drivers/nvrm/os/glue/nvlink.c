/*
 * The NVLink core's and NVSwitch library's OS calls, which RM imports
 * because nv-kernel.o carries both libraries (INCLUDE_NVLINK_LIB,
 * INCLUDE_NVSWITCH_LIB). A 3060 has neither NVLink nor an NVSwitch, and
 * nvrm starts neither library (docs/NVIDIA.md §4.2: "Stubs: ... NVSwitch,
 * NVLink"). The calls that are plain C-library or lock work are answered;
 * every call that would reach a fabric, a switch, DMA or an event is a
 * loud stub.
 *
 * Prototypes are NVIDIA's (export_nvswitch.h, nvlink_os.h, MIT, from the
 * fetched tree); the bodies are Ferrix's.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"
#include "export_nvswitch.h"
#include "nvlink_os.h"
#include "nvlink_errors.h"

#define NVLINK_WHY "NVLink and NVSwitch are stubbed on a GeForce (docs/NVIDIA.md §4.2)"

int nanosleep(const void *request, void *remain);
unsigned int geteuid(void);
int getpid(void);
char *strncpy(char *, const char *, size_t);
char *strcpy(char *, const char *);

/* ---- answered: the C library, locks, time ---------------------------- */

void *nvlink_malloc(NvLength size) { return malloc(size); }
void nvlink_free(void *pointer) { free(pointer); }
void *nvlink_memcpy(void *to, const void *from, NvLength size) { return memcpy(to, from, size); }
void *nvlink_memset(void *to, int value, NvLength size) { return memset(to, value, size); }
int nvlink_strcmp(const char *a, const char *b) { return strcmp(a, b); }
char *nvlink_strcpy(char *to, const char *from) { return strcpy(to, from); }
NvLength nvlink_strlen(const char *text) { return strlen(text); }
int nvlink_is_admin(void) { return os_is_administrator(); }
NvU64 nvlink_get_platform_time(void) { return os_get_monotonic_time_ns(); }
void nvlink_sleep(unsigned int ms) { os_delay(ms); }

void nvlink_assert(int expression)
{
    if (!expression)
        nvos_write_line("nvos: an NVLink core assertion failed\n");
}

void *nvlink_allocLock(void)
{
    return calloc(1, sizeof(nvos_mutex_t));
}

void nvlink_acquireLock(void *lock) { nvos_mutex_lock(lock); }
void nvlink_releaseLock(void *lock) { nvos_mutex_unlock(lock); }
void nvlink_freeLock(void *lock) { free(lock); }

void *nvswitch_os_malloc_trace(NvLength size, const char *file, NvU32 line) { return malloc(size); }
void nvswitch_os_free(void *pointer) { free(pointer); }
void *nvswitch_os_memcpy(void *to, const void *from, NvLength size) { return memcpy(to, from, size); }
void *nvswitch_os_memset(void *to, int value, NvLength size) { return memset(to, value, size); }
NvLength nvswitch_os_strlen(const char *text) { return strlen(text); }
int nvswitch_os_strncmp(const char *a, const char *b, NvLength length) { return strncmp(a, b, length); }
char *nvswitch_os_strncpy(char *to, const char *from, NvLength length) { return strncpy(to, from, length); }
int nvswitch_os_is_admin(void) { return os_is_administrator(); }
NvU64 nvswitch_os_get_platform_time(void) { return os_get_monotonic_time_ns(); }
void nvswitch_os_sleep(unsigned int ms) { os_delay(ms); }
NvU32 nvswitch_os_mem_read32(const volatile void *address) { return *(const volatile NvU32 *)address; }
void nvswitch_os_mem_write32(volatile void *address, NvU32 data) { *(volatile NvU32 *)address = data; }

NvU64 nvswitch_os_get_platform_time_epoch(void)
{
    NvU32 seconds, microseconds;

    os_get_system_time(&seconds, &microseconds);
    return (NvU64)seconds * 1000000000ULL + (NvU64)microseconds * 1000ULL;
}

NvlStatus nvswitch_os_get_pid(NvU32 *pPid)
{
    *pPid = os_get_current_process();
    return NVL_SUCCESS;
}

NvlStatus nvswitch_os_get_os_version(NvU32 *pMajorVer, NvU32 *pMinorVer, NvU32 *pBuildNum)
{
    os_version_info info;

    os_get_version_info(&info);
    *pMajorVer = info.os_major_version;
    *pMinorVer = info.os_minor_version;
    *pBuildNum = info.os_build_number;
    return NVL_SUCCESS;
}

int nvswitch_os_vsnprintf(char *buf, NvLength size, const char *fmt, va_list arglist)
{
    return vsnprintf(buf, size, fmt, arglist);
}

int nvswitch_os_snprintf(char *pString, NvLength size, const char *pFormat, ...)
{
    va_list arguments;
    int written;

    va_start(arguments, pFormat);
    written = vsnprintf(pString, size, pFormat, arguments);
    va_end(arguments);
    return written;
}

void nvswitch_os_print(int log_level, const char *pFormat, ...)
{
    char line[512];
    va_list arguments;

    va_start(arguments, pFormat);
    vsnprintf(line, sizeof(line), pFormat, arguments);
    va_end(arguments);
    nvos_write_line(line);
}

void nvswitch_os_assert_log(const char *pFormat, ...)
{
    char line[512];
    va_list arguments;

    va_start(arguments, pFormat);
    vsnprintf(line, sizeof(line), pFormat, arguments);
    va_end(arguments);
    nvos_write_line(line);
}

/* ---- loud stubs: what needs a fabric, a switch, DMA or events -------- */

void nvswitch_os_report_error(void *os_handle, NvU32 error_code, const char *fmt, ...)
{
    nvos_stub_called("nvswitch_os_report_error", NVLINK_WHY);
}

NvlStatus nvlink_acquire_fabric_mgmt_cap(void *a0, NvU64 a1)
{
    nvos_stub_called("nvlink_acquire_fabric_mgmt_cap", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

int nvlink_is_fabric_manager(void *a0)
{
    nvos_stub_called("nvlink_is_fabric_manager", NVLINK_WHY);
    return -1;
}

NvlStatus nvswitch_os_acquire_fabric_mgmt_cap(void *a0, NvU64 a1)
{
    nvos_stub_called("nvswitch_os_acquire_fabric_mgmt_cap", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_add_client_event(void *a0, void *a1, NvU32 a2)
{
    nvos_stub_called("nvswitch_os_add_client_event", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_alloc_contig_memory(void *a0, void **a1, NvU32 a2, NvBool a3)
{
    nvos_stub_called("nvswitch_os_alloc_contig_memory", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

void nvswitch_os_free_contig_memory(void *a0, void *a1, NvU32 a2)
{
    nvos_stub_called("nvswitch_os_free_contig_memory", NVLINK_WHY);
}

NvlStatus nvswitch_os_get_supported_register_events_params(NvBool *a0, NvBool *a1)
{
    nvos_stub_called("nvswitch_os_get_supported_register_events_params", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

int nvswitch_os_is_fabric_manager(void *a0)
{
    nvos_stub_called("nvswitch_os_is_fabric_manager", NVLINK_WHY);
    return -1;
}

NvBool nvswitch_os_is_uuid_in_blacklist(NvUuid *a0)
{
    nvos_stub_called("nvswitch_os_is_uuid_in_blacklist", NVLINK_WHY);
    return NV_FALSE;
}

NvlStatus nvswitch_os_map_dma_region(void *a0, void *a1, NvU64 *a2, NvU32 a3, NvU32 a4)
{
    nvos_stub_called("nvswitch_os_map_dma_region", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_notify_client_event(void *a0, void *a1, NvU32 a2)
{
    nvos_stub_called("nvswitch_os_notify_client_event", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

void nvswitch_os_override_platform(void *a0, NvBool *a1)
{
    nvos_stub_called("nvswitch_os_override_platform", NVLINK_WHY);
}

NvlStatus nvswitch_os_read_registry_dword(void *a0, const char *a1, NvU32 *a2)
{
    nvos_stub_called("nvswitch_os_read_registry_dword", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_remove_client_event(void *a0, void *a1)
{
    nvos_stub_called("nvswitch_os_remove_client_event", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_set_dma_mask(void *a0, NvU32 a1)
{
    nvos_stub_called("nvswitch_os_set_dma_mask", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_sync_dma_region_for_cpu(void *a0, NvU64 a1, NvU32 a2, NvU32 a3)
{
    nvos_stub_called("nvswitch_os_sync_dma_region_for_cpu", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_sync_dma_region_for_device(void *a0, NvU64 a1, NvU32 a2, NvU32 a3)
{
    nvos_stub_called("nvswitch_os_sync_dma_region_for_device", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

NvlStatus nvswitch_os_unmap_dma_region(void *a0, void *a1, NvU64 a2, NvU32 a3, NvU32 a4)
{
    nvos_stub_called("nvswitch_os_unmap_dma_region", NVLINK_WHY);
    return NVL_ERR_NOT_SUPPORTED;
}

