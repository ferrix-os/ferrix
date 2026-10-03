/*
 * RM's capabilities: on Linux, entries under /proc/driver/nvidia/capabilities
 * whose file descriptors a client presents to prove a right (MIG, fabric
 * management). RM makes its system capability at start (osRmCapRegisterSys),
 * so the entries are made here, as named records. No client can present one
 * before N1e, so validating a descriptor fails loudly.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"

struct nv_cap
{
    struct nv_cap *parent;
    int mode;
    NvBool directory;
    char name[64];
};

static nv_cap_t *nvos_cap_make(nv_cap_t *parent, const char *name, int mode, NvBool directory)
{
    nv_cap_t *cap = calloc(1, sizeof(*cap));

    if (cap == NULL)
        return NULL;
    cap->parent = parent;
    cap->mode = mode;
    cap->directory = directory;
    snprintf(cap->name, sizeof(cap->name), "%s", name);
    return cap;
}

nv_cap_t *nvos_caps_root_init(void)
{
    return nvos_cap_make(NULL, "driver/nvidia/capabilities", 0555, NV_TRUE);
}

nv_cap_t* NV_API_CALL os_nv_cap_create_dir_entry(nv_cap_t *parent_cap, const char *name, int mode)
{
    return nvos_cap_make(parent_cap, name, mode, NV_TRUE);
}

nv_cap_t* NV_API_CALL os_nv_cap_create_file_entry(nv_cap_t *parent_cap, const char *name, int mode)
{
    return nvos_cap_make(parent_cap, name, mode, NV_FALSE);
}

void NV_API_CALL os_nv_cap_destroy_entry(nv_cap_t *cap)
{
    free(cap);
}

int NV_API_CALL os_nv_cap_validate_and_dup_fd(const nv_cap_t *cap, int fd)
{
    nvos_stub_called("os_nv_cap_validate_and_dup_fd",
                     "a capability descriptor needs request_file (N1e)");
    return -1;
}

void NV_API_CALL os_nv_cap_close_fd(int fd)
{
    nvos_stub_called("os_nv_cap_close_fd", "no capability descriptor was ever duplicated");
}
