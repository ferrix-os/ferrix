/*
 * nvrm-trip: what one request into nvrm costs, by stage (docs/NVIDIA.md
 * §4.7). The init of `cargo xtask test-nvrm`'s handed-over boot, and a
 * program to run on a card: it needs only /dev/nvidiactl.
 *
 *   nvrm-trip [COUNT] [control]
 *
 * It waits for /dev/nvidiactl, then times COUNT (default 5000) of each
 * of:
 *
 *  - the version query (NV_ESC_CHECK_VERSION_STR): 72 bytes in and out,
 *    no lock of RM's -- the floor;
 *  - a null control (NV_ESC_RM_CONTROL, command 0) on a root client of its
 *    own: RM's API lock and its client lookup, 32 bytes in and out.
 *
 * After each it asks nvrm for that request's own stamps (NVRM_TRIP_STAMPS,
 * os/glue/chardev.c), on the same time-stamp counter, so each round trip
 * splits in four: to nvrm (the ioctl's entry, the kernel's chardev core,
 * nvrm's wake), the hand-off inside nvrm, the handler (RM's work and its
 * copies of this program's memory), and back (the answer, this program's
 * wake). One line a request, medians throughout.
 *
 * Each line begins with how nvrm served, as its stamps say: `pool` or
 * `thread-a-request`. With `control`, or as init, and nvrm serving from
 * its pool, it then asks nvrm to start a thread for every request, as it
 * did before the pool (NVRM_TRIP_MODE 1, which nvrm takes on the test
 * device only), and times both again: the gate's negative control.
 *
 * As init it never exits: the kernel would power off.
 */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define NV_IOWR(nr, size) (0xC0000000u | ((uint32_t)(size) << 16) | ('F' << 8) | (nr))
#define NV_ESC_CHECK_VERSION_STR 210
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_RM_CONTROL 0x2A
#define NV_ESC_RM_ALLOC 0x2B
#define NV01_ROOT 0x0

struct version { uint32_t cmd, reply; char text[64]; };
struct os21 { uint32_t root, parent, new, class; uint64_t params; uint32_t size, status; };
struct os54 { uint32_t client, object, cmd, flags; uint64_t params; uint32_t size, status; };
struct os00 { uint32_t root, parent, old, status; };

struct stamps { uint64_t id, taken, serving, done; uint32_t cmd, copies, pid, mode; };
#define TRIP_STAMPS (0x80000000u | ((uint32_t)sizeof(struct stamps) << 16) | (0xF7 << 8) | 1)
#define TRIP_MODE (0x40000000u | (4u << 16) | (0xF7 << 8) | 2)

static inline uint64_t tsc(void)
{
	uint32_t low, high;
	__asm__ volatile("lfence; rdtsc" : "=a"(low), "=d"(high));
	return ((uint64_t)high << 32) | low;
}

static uint64_t now_ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

static int ascending(const void *a, const void *b)
{
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return x < y ? -1 : x > y;
}

static double scale; /* ns a tick */
static int ctl;
/* How nvrm served the last request stamped: NVRM_TRIP_MODE's. */
static uint32_t served;

enum { WHOLE, TO, HAND, HANDLER, BACK, SPANS };
static uint64_t *span[SPANS];

static unsigned long at(int which, int count, int hundredth)
{
	return (unsigned long)((double)span[which][(long)count * hundredth / 100] * scale);
}

/* Time `count` of one request, `call` making it, and say its line. */
static void run(const char *name, int count, int (*call)(void))
{
	const char *mode;
	int kept = 0, failed = 0;
	uint32_t copies = 0;
	for (int i = -200; i < count; i++) {
		uint64_t before = tsc();
		int rc = call();
		uint64_t after = tsc();
		struct stamps s;
		memset(&s, 0, sizeof s);
		if (rc != 0 || ioctl(ctl, TRIP_STAMPS, &s) != 0 || s.id == 0 ||
		    s.taken < before || s.serving < s.taken || s.done < s.serving ||
		    after < s.done) {
			failed++;
			continue;
		}
		if (i < 0)
			continue;
		span[WHOLE][kept] = after - before;
		span[TO][kept] = s.taken - before;
		span[HAND][kept] = s.serving - s.taken;
		span[HANDLER][kept] = s.done - s.serving;
		span[BACK][kept] = after - s.done;
		copies = s.copies;
		served = s.mode;
		kept++;
	}
	mode = served == 0 ? "pool" : "thread-a-request";
	if (kept < count / 2) {
		printf("nvrm-trip: %s %s: stopped: only %d of %d round trips were stamped in order (%d not)\n",
		       mode, name, kept, count, failed);
		return;
	}
	for (int which = 0; which < SPANS; which++)
		qsort(span[which], (size_t)kept, sizeof(uint64_t), ascending);
	printf("nvrm-trip: %s %s: n %d, round trip p50 %lu ns (min %lu, p90 %lu, p99 %lu); "
	       "to nvrm %lu, hand-off %lu, handler %lu (%u copies), back %lu\n",
	       mode, name, kept, at(WHOLE, kept, 50), (unsigned long)((double)span[WHOLE][0] * scale),
	       at(WHOLE, kept, 90), at(WHOLE, kept, 99), at(TO, kept, 50), at(HAND, kept, 50),
	       at(HANDLER, kept, 50), copies, at(BACK, kept, 50));
}

static int version_query(void)
{
	struct version v;
	memset(&v, 0, 8);
	v.cmd = '2';
	return ioctl(ctl, NV_IOWR(NV_ESC_CHECK_VERSION_STR, sizeof v), &v);
}

static uint32_t client;

static int null_control(void)
{
	struct os54 c;
	memset(&c, 0, sizeof c);
	c.client = client;
	c.object = client;
	int rc = ioctl(ctl, NV_IOWR(NV_ESC_RM_CONTROL, sizeof c), &c);
	return rc != 0 ? rc : (int)c.status;
}

static void both(int count)
{
	run("version-query", count, version_query);
	if (client != 0)
		run("null-control", count, null_control);
}

int main(int argc, char **argv)
{
	int init = getpid() == 1;
	int count = !init && argc > 1 ? atoi(argv[1]) : 5000;
	int control = init || (argc > 2 && strcmp(argv[2], "control") == 0);
	if (count < 100)
		count = 100;

	for (int waited = 0; (ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC)) < 0; waited++) {
		if (!init || waited >= 1200) {
			printf("nvrm-trip: stopped: no /dev/nvidiactl\n");
			goto out;
		}
		usleep(100000);
	}
	for (int which = 0; which < SPANS; which++) {
		span[which] = malloc(sizeof(uint64_t) * (size_t)count);
		if (span[which] == NULL) {
			printf("nvrm-trip: stopped: no memory\n");
			goto out;
		}
	}
	uint64_t ticks = tsc(), nanos = now_ns();
	usleep(50000);
	scale = (double)(now_ns() - nanos) / (double)(tsc() - ticks);
	printf("nvrm-trip: /dev/nvidiactl open; the counter runs at %lu MHz\n",
	       (unsigned long)(1000.0 / scale));

	uint64_t before = tsc();
	for (int i = 0; i < 10000; i++)
		(void)getppid();
	printf("nvrm-trip: a bare system call: %lu ns\n",
	       (unsigned long)((double)(tsc() - before) * scale / 10000.0));

	struct os21 root;
	memset(&root, 0, sizeof root);
	root.class = NV01_ROOT;
	if (ioctl(ctl, NV_IOWR(NV_ESC_RM_ALLOC, sizeof root), &root) != 0 || root.status != 0 ||
	    root.new == 0)
		printf("nvrm-trip: no root client (status 0x%x): the null control is left out\n",
		       root.status);
	else
		client = root.new;

	both(count);
	if (control && served == 0) {
		uint32_t mode = 1;
		if (ioctl(ctl, TRIP_MODE, &mode) != 0) {
			printf("nvrm-trip: control: nvrm does not take the mode here\n");
		} else {
			both(count < 2000 ? count : 2000);
			mode = 0;
			(void)ioctl(ctl, TRIP_MODE, &mode);
		}
	}
	if (client != 0) {
		struct os00 gone = { client, client, client, 0 };
		(void)ioctl(ctl, NV_IOWR(NV_ESC_RM_FREE, sizeof gone), &gone);
	}
	printf("nvrm-trip: done\n");
out:
	fflush(stdout);
	while (init)
		sleep(3600);
	return 0;
}
