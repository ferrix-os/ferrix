/*
 * nvrm-link-test: RM's core, loaded and run with no GPU (docs/NVIDIA.md
 * §7, N1c). The same objects as nvrm, with this main in place of nvrm's:
 * it is an ordinary program (crt1.o, argv), so it runs on the host as on
 * Ferrix, and it has its own core, linked against its own addresses.
 *
 *   nvrm-link-test CORE                 load CORE, then RM's no-GPU path
 *   nvrm-link-test --control-rwx        the maps check's negative control
 *
 * The no-GPU path is what a Linux client's first calls reach: RM's
 * initialisation (nvrm_module_init: rm_init_rm, the control device's
 * state, the registry), the control device opened, a root client
 * allocated, and NV01_DEVICE_0 asked for, which RM must refuse, since no
 * GPU is attached; then the client freed and RM shut down. Every step is a
 * line; the last is "nvrm-link-test: passed". This program is its own
 * client: its ioctl arguments are its own memory (os/nvos/src/client.rs).
 *
 * The exit status is 0, a loader refusal's (os/nvos/src/rmcore.rs: 20 and
 * up), or the step that failed.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"
#include "class/cl0080.h"

#include "core-calls.h"

int strcmp(const char *, const char *);

/* From pin.c, core-calls.S and ferrix-nvos (os/nvos/src/rmcore.rs). */
extern const unsigned char nvrm_core_sha256[32];
int nvos_core_load(const char *path, const unsigned char (*pin)[32],
		   unsigned long long *table, NvU32 exports, NvU32 data);
int nvos_core_control_rwx(void);

/* nv_escape.h's (src/nvidia/arch/nvalloc/unix/include), and nvos.h's
 * NVOS21_PARAMETERS and NVOS00_PARAMETERS, whose name this layer's own
 * nvos.h shadows. RM refuses either at another size. */
#define NV_ESC_RM_FREE  0x29
#define NV_ESC_RM_ALLOC 0x2B
#define NV01_ROOT       0x0

typedef struct {
	NvHandle hRoot;
	NvHandle hObjectParent;
	NvHandle hObjectNew;
	NvV32 hClass;
	NvP64 pAllocParms NV_ALIGN_BYTES(8);
	NvU32 paramsSize;
	NvV32 status;
} link_alloc_t;

typedef struct {
	NvHandle hRoot;
	NvHandle hObjectParent;
	NvHandle hObjectOld;
	NvV32 status;
} link_free_t;

_Static_assert(sizeof(link_alloc_t) == 32, "NVOS21_PARAMETERS is 32 bytes");
_Static_assert(sizeof(link_free_t) == 16, "NVOS00_PARAMETERS is 16 bytes");

/* The device handle the test asks for. */
#define DEVICE_HANDLE 0xcaf00001u

enum step {
	STEP_USAGE = 2,
	STEP_CONTROL = 3,
	STEP_CLIENT = 4,
	STEP_INIT = 5,
	STEP_OPEN = 6,
	STEP_ROOT = 7,
	STEP_DEVICE = 8,
	STEP_FREE = 9,
};

/* This program as its own client: its "user" memory is its own. */
static int copy_in(void *context, void *to, NvU64 from, NvU32 length)
{
	(void)context;
	memcpy(to, (const void *)(NvUPtr)from, length);
	return 0;
}

static int copy_out(void *context, NvU64 to, const void *from, NvU32 length)
{
	(void)context;
	memcpy((void *)(NvUPtr)to, from, length);
	return 0;
}

static const nvos_client_t self = {
	.pid = 1,
	.euid = 0,
	.administrator = NV_TRUE,
	.name = "nvrm-link-test",
	.copy_in = copy_in,
	.copy_out = copy_out,
	.context = NULL,
};

static int stop(enum step step, const char *what)
{
	nv_printf(NV_DBG_ERRORS, "nvrm-link-test: stopped: %s\n", what);
	return step;
}

/* One RM call on the control device, as a client's ioctl reaches it. */
static int call(nv_linux_file_private_t *ctl, unsigned int escape, void *arg,
		unsigned int size)
{
	return nvrm_ioctl(ctl, _IOC(3U, NV_IOCTL_MAGIC, escape, size), arg);
}

int main(int argc, char **argv)
{
	if (argc == 2 && strcmp(argv[1], "--control-rwx") == 0) {
		int refused = nvos_core_control_rwx();
		nv_printf(NV_DBG_ERRORS,
			  "nvrm-link-test: control: the maps check answered %d\n",
			  refused);
		return refused == 0 ? STEP_CONTROL : refused;
	}
	if (argc != 2)
		return stop(STEP_USAGE, "usage: nvrm-link-test CORE | --control-rwx");

	int status = nvos_core_load(argv[1], &nvrm_core_sha256,
				    nvrm_core_table, NVRM_CORE_EXPORTS,
				    NVRM_CORE_DATA);
	if (status != 0)
		return status;

	if (!nvos_client_enter(&self))
		return stop(STEP_CLIENT, "nvos_client_enter refused");

	int rc = nvrm_module_init();
	if (rc != 0) {
		nv_printf(NV_DBG_ERRORS, "nvrm-link-test: nvrm_module_init: %d\n", rc);
		return stop(STEP_INIT, "RM did not initialise");
	}
	nv_printf(NV_DBG_ERRORS, "nvrm-link-test: RM initialised: %s\n", pNVRM_ID);

	nv_linux_file_private_t *ctl = nvrm_open_ctl();
	if (ctl == NULL)
		return stop(STEP_OPEN, "the control device did not open");

	link_alloc_t root = { 0 };
	root.hClass = NV01_ROOT;
	rc = call(ctl, NV_ESC_RM_ALLOC, &root, sizeof root);
	if (rc != 0 || root.status != NV_OK || root.hObjectNew == 0) {
		nv_printf(NV_DBG_ERRORS,
			  "nvrm-link-test: root client: ioctl %d, status 0x%x\n",
			  rc, root.status);
		return stop(STEP_ROOT, "no root client");
	}
	NvHandle client = root.hObjectNew;
	nv_printf(NV_DBG_ERRORS, "nvrm-link-test: root client 0x%x allocated\n",
		  client);

	NV0080_ALLOC_PARAMETERS parameters = { 0 };
	parameters.deviceId = 0;
	link_alloc_t device = { 0 };
	device.hRoot = client;
	device.hObjectParent = client;
	device.hObjectNew = DEVICE_HANDLE;
	device.hClass = NV01_DEVICE_0;
	device.pAllocParms = &parameters;
	device.paramsSize = sizeof parameters;
	rc = call(ctl, NV_ESC_RM_ALLOC, &device, sizeof device);
	if (rc != 0 || device.status == NV_OK)
		return stop(STEP_DEVICE, "NV01_DEVICE_0 was not refused with no GPU");
	nv_printf(NV_DBG_ERRORS,
		  "nvrm-link-test: NV01_DEVICE_0 refused with no GPU: status 0x%x\n",
		  device.status);

	link_free_t free_client = { client, client, client, 0 };
	rc = call(ctl, NV_ESC_RM_FREE, &free_client, sizeof free_client);
	if (rc != 0 || free_client.status != NV_OK)
		return stop(STEP_FREE, "the root client was not freed");

	nvrm_close(ctl);
	nvos_client_leave();
	nvrm_module_exit();
	nv_printf(NV_DBG_ERRORS, "nvrm-link-test: passed\n");
	return 0;
}
