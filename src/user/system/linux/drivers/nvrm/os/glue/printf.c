/*
 * RM's printing calls, which take a format and arguments, so they are C:
 * nv_printf, out_string, os_snprintf, and the debug level that filters
 * them (docs/NVIDIA.md §4.2, "logging"). Each line goes to the console
 * through ferrix-nvos (nvos_write_line). The level rule is NVIDIA's
 * os-interface.c's: print a message whose level is at least bits 5:4 of
 * cur_debuglevel, by default warnings and errors.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"

/* NVIDIA's default: everything, which bits 5:4 read as warnings and up. */
NvU32 cur_debuglevel = 0xffffffff;

/* Lines may be built from several nv_printf calls; one at a time. */
static nvos_mutex_t print_lock;

static int nvos_vprint(const char *format, va_list arguments)
{
    char line[1024];
    int written;

    written = vsnprintf(line, sizeof(line), format, arguments);
    if (written < 0)
        return written;
    nvos_mutex_lock(&print_lock);
    nvos_write_line(line);
    nvos_mutex_unlock(&print_lock);
    return written;
}

int NV_API_CALL nv_printf(NvU32 debuglevel, const char *printf_format, ...)
{
    va_list arglist;
    int chars_written = 0;

    if (debuglevel >= ((cur_debuglevel >> 4) & 0x3))
    {
        va_start(arglist, printf_format);
        chars_written = nvos_vprint(printf_format, arglist);
        va_end(arglist);
    }

    return chars_written;
}

void NV_API_CALL out_string(const char *str)
{
    nvos_mutex_lock(&print_lock);
    nvos_write_line(str);
    nvos_mutex_unlock(&print_lock);
}

NvS32 NV_API_CALL os_snprintf(char *buf, NvU32 size, const char *fmt, ...)
{
    va_list arglist;
    int chars_written;

    va_start(arglist, fmt);
    chars_written = vsnprintf(buf, size, fmt, arglist);
    va_end(arglist);

    return chars_written;
}

/* NVIDIA's os_dbg_init: the registry's ResmanDebugLevel, if set. */
void NV_API_CALL os_dbg_init(void)
{
    NvU32 new_debuglevel;
    nvidia_stack_t *sp = NULL;

    if (nv_kmem_cache_alloc_stack(&sp) != 0)
    {
        return;
    }

    if (NV_OK == rm_read_registry_dword(sp, NULL,
                                        "ResmanDebugLevel",
                                        &new_debuglevel))
    {
        if (new_debuglevel != (NvU32)~0)
            cur_debuglevel = new_debuglevel;
    }

    nv_kmem_cache_free_stack(sp);
}

void NV_API_CALL os_dbg_set_level(NvU32 new_debuglevel)
{
    nv_printf(NV_DBG_SETUP, "NVRM: Changing debuglevel from 0x%x to 0x%x\n",
        cur_debuglevel, new_debuglevel);
    cur_debuglevel = new_debuglevel;
}
