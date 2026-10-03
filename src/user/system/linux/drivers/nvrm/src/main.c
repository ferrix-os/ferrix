/*
 * nvrm: the ring-3 program that will host NVIDIA's resource manager
 * (docs/NVIDIA.md §4.1). This is N1b's skeleton, and no NVIDIA code is in
 * it yet: it shows that devmgr hands it a GPU only as §12.2 and §12.3
 * require, and that from here it can reach the device and run threads.
 *
 * devmgr has set the device's isolated-interrupts mark, found its
 * interrupts isolated and set its pin budget before starting this
 * (ferrix_devmgr_proto::gpu). nvrm then:
 *
 *  1. reads START and its device from the bootstrap channel, as every
 *     driver devmgr starts does;
 *  2. says what the device is (device_info);
 *  3. reads device_isolation, prints it, and refuses the device unless
 *     bit 1 (interrupts isolated) holds: the second guard after devmgr's,
 *     and the line that verifies F-57's closure on the ferrix-3060 domain
 *     at N1's first boot;
 *  4. prints its pin budget (device_get_limit);
 *  4a. loads RM's core from the NVIDIA volume (docs/NVIDIA.md §4.1, "The
 *     core"): it waits, a bounded while, for the file to appear, since the
 *     volume is mounted after devmgr reports, and nothing waits on nvrm;
 *     then nvos_core_load checks and maps it, or refuses with its line.
 *     device_isolation is printed before, so F-57's first-boot record is
 *     had even when the load fails;
 *  5. lists the apertures whole (device_aperture) and reads the vendor and
 *     device back through the configuration window;
 *  6. maps BAR0 and reads its first register: NV_PMC_BOOT_0, the chip's
 *     boot identity, on an NVIDIA GPU; on the test device any register;
 *  7. runs a thread and joins it, since RM needs threads (§4.1);
 *  7a. on an NVIDIA GPU (N1d): attaches the device to ferrix-nvos,
 *     starts RM (nvrm_module_init) and probes and starts the GPU
 *     (os/kept/nv-pci.c), through rm_init_adapter, which boots its GSP;
 *     the test device has no RM to start and skips this;
 *  8. says it is up, and sleeps until it is killed.
 *
 * Every line goes to standard error, which is the console, in one write.
 * A step that fails says so and exits with its number.
 */
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "core-calls.h"
#include "native.h"

/* NVIDIA's PCI vendor. */
#define NVIDIA_VENDOR 0x10de

/* Where nvrm gave up, as its exit status. */
enum step {
	STEP_BOOTSTRAP = 2,
	STEP_START = 3,
	STEP_INFO = 4,
	STEP_ISOLATION = 5,
	STEP_UNISOLATED = 6,
	STEP_BUDGET = 7,
	STEP_APERTURE = 8,
	STEP_CONFIG = 9,
	STEP_BAR0 = 10,
	STEP_THREAD = 11,
	STEP_VOLUME = 12,
	STEP_ATTACH = 13,
	STEP_RM = 14,
	STEP_CHARDEV = 15,
};

/* Where the NVIDIA volume, mounted at /data, carries RM's core (written
 * there by xtask beside the nvrm it was linked for, not by
 * fetch-nvidia.sh), and how long nvrm waits for it. */
#define CORE_PATH "/data/usr/lib/ferrix/nvrm-core"
#define VOLUME_PATIENCE_MS 60000
#define VOLUME_POLL_MS 250

/* From pin.c, and the call table core-calls.S jumps through. */
extern const unsigned char nvrm_core_sha256[32];
extern unsigned long long nvrm_core_table[];

/* From ferrix-nvos (os/nvos/src/rmcore.rs): 0, or the refusal's status
 * after its line. */
extern int nvos_core_load(const char *path, const unsigned char (*pin)[32],
			  unsigned long long *table, uint32_t exports,
			  uint32_t data);

/* From start.c. */
extern uint32_t nvrm_bootstrap;

/* ferrix-nvos (include/nvos.h) and the kept C (include/nv-ferrix.h), which
 * this file, built against ferrousli's headers alone, declares itself. */
extern uint32_t nvos_device_attach(uint32_t handle);
extern int nvrm_module_init(void);
extern int nvrm_gpu_start(void);
extern int nvrm_chardev_serve(const uint16_t *minors, uint32_t count);

/* The device's place, as devmgr writes it: bb:dd.f. */
static char place[16];

/* One line on the console, in one write. */
__attribute__((format(printf, 1, 2))) static void say(const char *format, ...)
{
	char line[256];
	int used = snprintf(line, sizeof line, "nvrm: ");
	va_list arguments;
	va_start(arguments, format);
	int more = vsnprintf(line + used, sizeof line - (size_t)used - 1, format,
			     arguments);
	va_end(arguments);
	if (more < 0)
		more = 0;
	used += more;
	if ((size_t)used > sizeof line - 2)
		used = sizeof line - 2;
	line[used++] = '\n';
	(void)write(2, line, (size_t)used);
}

/* Say why, and exit with the step. */
static int stop(enum step step, const char *what, long status)
{
	say("stopped: %s (status %ld)", what, status);
	return (int)step;
}

/* Wait for the bootstrap channel to carry START, and take the device. */
static long receive(uint32_t *device)
{
	uint32_t observed = 0;
	long result = nv_call(NV_OBJECT_WAIT_ONE, nvrm_bootstrap,
			      NV_SIGNAL_READABLE | NV_SIGNAL_PEER_CLOSED, 0,
			      (long)&observed, 0, 0);
	if (nv_failed(result))
		return result;
	unsigned char start[256];
	uint32_t handles[NV_CHANNEL_MAX_HANDLES];
	uint32_t actual[2] = { 0, 0 };
	result = nv_call(NV_CHANNEL_READ, nvrm_bootstrap, (long)start,
			 sizeof start, (long)handles, NV_CHANNEL_MAX_HANDLES,
			 (long)actual);
	if (nv_failed(result))
		return result;
	/* START, in the block ring's layout, and the device alone. */
	if (actual[0] == 0 || actual[1] != 1)
		return -1;
	*device = handles[0];
	return 0;
}

/* The thread's work: to have run. */
static void *ran(void *flag)
{
	*(volatile int *)flag = 1;
	return flag;
}

/* Map aperture `index` and read its first register. */
static int bar0(uint32_t device, const struct nv_device_info *info)
{
	struct nv_aperture_info aperture;
	for (uint32_t index = 0; index < info->apertures; index++) {
		memset(&aperture, 0, sizeof aperture);
		long result = nv_call(NV_DEVICE_APERTURE, device, index,
				      (long)&aperture, 0, 0, 0);
		if (nv_failed(result))
			return stop(STEP_APERTURE, "device_aperture refused",
				    result);
		if (aperture.bar != 0)
			continue;
		if (!(aperture.flags & NV_APERTURE_WHOLE_PAGES))
			return stop(STEP_BAR0, "BAR0 is not whole pages", 0);
		struct nv_io_mapping_spec spec = { aperture.phys, aperture.len };
		long mapping = nv_call(NV_IO_MAPPING_CREATE, device,
				       (long)&spec, 0, 0, 0, 0);
		if (nv_failed(mapping))
			return stop(STEP_BAR0, "io_mapping_create refused BAR0",
				    mapping);
		long at = nv_call(NV_IO_MAPPING_MAP, mapping, 0, 0, 0, 0, 0);
		if (nv_failed(at))
			return stop(STEP_BAR0, "io_mapping_map refused BAR0", at);
		uint32_t value = *(volatile const uint32_t *)at;
		if (info->vendor_id == NVIDIA_VENDOR)
			say("BAR0 mapped, %llu KiB; NV_PMC_BOOT_0 reads 0x%08x",
			    (unsigned long long)(aperture.len >> 10), value);
		else
			say("BAR0 mapped, %llu KiB; its register 0x0 reads "
			    "0x%08x (the test device: no NV_PMC_BOOT_0)",
			    (unsigned long long)(aperture.len >> 10), value);
		return 0;
	}
	return stop(STEP_BAR0, "the device has no BAR0 aperture", 0);
}

/* Wait for the core's file, a bounded while, then load it. */
static int load_core(void)
{
	int waited = 0;
	while (access(CORE_PATH, R_OK) != 0) {
		if (waited == 0)
			say("waiting up to %d s for the NVIDIA volume (%s)",
			    VOLUME_PATIENCE_MS / 1000, CORE_PATH);
		if (waited >= VOLUME_PATIENCE_MS) {
			say("stopped: no NVIDIA volume within %d s: %s is "
			    "missing", VOLUME_PATIENCE_MS / 1000, CORE_PATH);
			return STEP_VOLUME;
		}
		usleep(VOLUME_POLL_MS * 1000);
		waited += VOLUME_POLL_MS;
	}
	return nvos_core_load(CORE_PATH, &nvrm_core_sha256, nvrm_core_table,
			      NVRM_CORE_EXPORTS, NVRM_CORE_DATA);
}

int main(void)
{
	uint32_t device = 0;
	int status;
	long result = receive(&device);
	if (nv_failed(result))
		return stop(STEP_BOOTSTRAP, "no START on the bootstrap channel",
			    result);
	if (result != 0)
		return stop(STEP_START, "START did not carry one device", result);

	struct nv_device_info info;
	memset(&info, 0, sizeof info);
	result = nv_call(NV_DEVICE_INFO, device, (long)&info, 0, 0, 0, 0);
	if (nv_failed(result))
		return stop(STEP_INFO, "device_info refused", result);
	snprintf(place, sizeof place, "%02x:%02x.%u", (info.location >> 8) & 0xff,
		 (info.location >> 3) & 0x1f, info.location & 7);
	say("started on %s, %04x:%04x class %06x, %u apertures, %u vectors; "
	    "its device over bootstrap",
	    place, info.vendor_id, info.device_id, info.class_code,
	    info.apertures, info.vectors);

	long isolation = nv_call(NV_DEVICE_ISOLATION, device, 0, 0, 0, 0, 0);
	if (nv_failed(isolation))
		return stop(STEP_ISOLATION, "device_isolation refused", isolation);
	if (!(isolation & NV_DEVICE_ISOLATION_INTERRUPTS)) {
		say("device_isolation %#lx: interrupts NOT isolated; refusing "
		    "the device", isolation);
		return STEP_UNISOLATED;
	}
	say("device_isolation %#lx: interrupts isolated (bit 1), DMA %s",
	    isolation,
	    (isolation & NV_DEVICE_ISOLATION_DMA_TRANSLATED) ? "translated"
							      : "not yet translated");

	long budget = nv_call(NV_DEVICE_GET_LIMIT, device,
			      NV_DEVICE_LIMIT_PIN_PAGES, 0, 0, 0, 0);
	long marked = nv_call(NV_DEVICE_GET_LIMIT, device,
			      NV_DEVICE_LIMIT_ISOLATED_INTERRUPTS, 0, 0, 0, 0);
	if (nv_failed(budget) || nv_failed(marked))
		return stop(STEP_BUDGET, "device_get_limit refused",
			    nv_failed(budget) ? budget : marked);
	say("pin budget %ld pages (%ld MiB), isolated-interrupts mark %ld",
	    budget, budget / 256, marked);

	status = load_core();
	if (status != 0)
		return status;

	for (uint32_t index = 0; index < info.apertures; index++) {
		struct nv_aperture_info aperture;
		memset(&aperture, 0, sizeof aperture);
		result = nv_call(NV_DEVICE_APERTURE, device, index,
				 (long)&aperture, 0, 0, 0);
		if (nv_failed(result))
			return stop(STEP_APERTURE, "device_aperture refused",
				    result);
		say("aperture %u: BAR %u, %#llx, %llu KiB%s%s", index,
		    aperture.bar, (unsigned long long)aperture.phys,
		    (unsigned long long)(aperture.len >> 10),
		    (aperture.flags & NV_APERTURE_BAR_64) ? ", 64-bit" : "",
		    (aperture.flags & NV_APERTURE_PREFETCHABLE) ? ", prefetchable"
								 : "");
	}

	uint32_t identity = 0;
	result = nv_call(NV_DEVICE_CONFIG_READ, device, 0, 4, (long)&identity,
			 0, 0);
	if (nv_failed(result))
		return stop(STEP_CONFIG, "device_config_read refused", result);
	say("configuration window: vendor %04x device %04x",
	    identity & 0xffff, identity >> 16);

	status = bar0(device, &info);
	if (status != 0)
		return status;

	pthread_t thread;
	volatile int flag = 0;
	void *joined = 0;
	if (pthread_create(&thread, 0, ran, (void *)&flag) != 0 ||
	    pthread_join(thread, &joined) != 0 || !flag)
		return stop(STEP_THREAD, "a thread did not run", 0);
	say("a thread ran and was joined");

	if (info.vendor_id == NVIDIA_VENDOR) {
		uint32_t attached = nvos_device_attach(device);
		if (attached != 0)
			return stop(STEP_ATTACH, "nvos_device_attach refused",
				    attached);
		int rc = nvrm_module_init();
		if (rc != 0)
			return stop(STEP_RM, "RM did not initialise", rc);
		say("RM initialised; probing the GPU");
		status = nvrm_gpu_start();
		if (status != 0) {
			say("stopped: the GPU did not start (step %d)", status);
			return status;
		}
		say("GPU started on %s", place);

		/* N1e: /dev/nvidiactl and /dev/nvidia0, through the kernel's
		 * chardev core; this serves them for nvrm's life. */
		static const uint16_t minors[] = { 255, 0 };
		int served = nvrm_chardev_serve(minors, 2);
		return stop(STEP_CHARDEV, "the device files' control failed",
			    served);
	}

	say("skeleton up on %s; idle", place);
	for (;;)
		sleep(3600);
}
