/*
 * rig: the game path's rig programs that are not the frame recorder
 * (docs/GAME-PATH.md §2.4, stage 24's R3). One static binary, the same
 * on the Linux guest and on Ferrix, so that nothing is measured with one
 * tool on one system and another on the other.
 *
 *   rig cpus                       how many processors /proc/stat lists
 *   rig busy SECONDS               each processor's busy share over SECONDS
 *   rig wake [-i US] [-n COUNT] [-d US]
 *                                  M5/M9: sleep to an absolute CLOCK_MONOTONIC
 *                                  time every US microseconds (1000), COUNT
 *                                  times (10000), and report the overshoot;
 *                                  -d spins US more after each wake-up before
 *                                  the clock is read (the negative control)
 *   rig hold [PERCENT]             hold memory until PERCENT (80) of it is in
 *                                  use, and keep it there until killed
 *   rig write DIRECTORY [MIB]      stream MIB (1024) MiB to DIRECTORY/rig-write.bin,
 *                                  fsync, start again, until killed
 *   rig launch PROGRAM [ARGS...]   M6: run PROGRAM and report, from just
 *                                  before its exec, the preload library's
 *                                  constructor and its first swap's return
 *   rig fake-game [MS]             a stand-in for a game under `rig launch`:
 *                                  reports a constructor at once and a first
 *                                  swap MS (0) milliseconds later
 *
 * The launch protocol, which the frame recorder's preload library (R1)
 * implements: `rig launch` passes RIG_LAUNCH_FD, the write end of a pipe,
 * and RIG_LAUNCH_T0_NS, CLOCK_MONOTONIC in nanoseconds just before the
 * exec. The library takes CLOCK_MONOTONIC in its constructor and again when
 * the first swap returns, and writes one line to that descriptor,
 *
 *   ctor <ns> swap <ns>\n
 *
 * then closes it. Without RIG_LAUNCH_FD in its environment it writes
 * nothing. Every line `rig` prints for a gate to read starts with the
 * subcommand's name and a colon.
 *
 * Build: cc -O2 -static -o rig rig.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAX_CPUS 256

static int64_t now_ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

/* One processor's ticks: busy (everything but idle, iowait and steal),
 * the whole, and steal. */
struct ticks {
	unsigned long long busy, total, steal;
};

/* Read /proc/stat's cpuN lines into `out`; returns how many. */
static int read_stat(struct ticks *out)
{
	FILE *f = fopen("/proc/stat", "r");
	char line[512];
	int n = 0;
	if (!f) {
		perror("rig: /proc/stat");
		exit(2);
	}
	while (fgets(line, sizeof line, f) && n < MAX_CPUS) {
		unsigned long long v[10] = { 0 };
		int cpu;
		if (strncmp(line, "cpu", 3) != 0 || line[3] < '0' || line[3] > '9')
			continue;
		if (sscanf(line, "cpu%d %llu %llu %llu %llu %llu %llu %llu %llu", &cpu, &v[0],
			   &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7]) < 5)
			continue;
		/* user nice system idle iowait irq softirq steal; guest and
		 * guest_nice are already in user and nice. */
		out[n].total = v[0] + v[1] + v[2] + v[3] + v[4] + v[5] + v[6] + v[7];
		out[n].busy = v[0] + v[1] + v[2] + v[5] + v[6];
		out[n].steal = v[7];
		n++;
	}
	fclose(f);
	return n;
}

static int cmd_cpus(void)
{
	struct ticks t[MAX_CPUS];
	printf("%d\n", read_stat(t));
	return 0;
}

static int cmd_busy(int argc, char **argv)
{
	struct ticks a[MAX_CPUS], b[MAX_CPUS];
	int seconds = argc > 2 ? atoi(argv[2]) : 10;
	int n = read_stat(a);
	double min = 101, sum = 0;
	int min_cpu = -1;
	sleep(seconds > 0 ? seconds : 10);
	if (read_stat(b) != n) {
		fprintf(stderr, "rig: the processor count changed\n");
		return 1;
	}
	for (int i = 0; i < n; i++) {
		unsigned long long total = b[i].total - a[i].total;
		unsigned long long busy = b[i].busy - a[i].busy;
		unsigned long long steal = b[i].steal - a[i].steal;
		/* Of the time the processor ran: steal is the host's, not
		 * the load's, and is printed beside the share. */
		double share = total > steal ? 100.0 * busy / (total - steal) : 0;
		printf("busy: cpu%d %.1f%% of %llu ticks, steal %llu\n", i, share, total, steal);
		if (share < min) {
			min = share;
			min_cpu = i;
		}
		sum += share;
	}
	printf("busy: %d processors over %d s, lowest %.1f%% (cpu%d), mean %.1f%%\n", n, seconds,
	       min, min_cpu, n ? sum / n : 0);
	return 0;
}

static int compare(const void *a, const void *b)
{
	int64_t x = *(const int64_t *)a, y = *(const int64_t *)b;
	return x < y ? -1 : x > y;
}

/* The value at percentile p (0 to 100) of n sorted samples. */
static int64_t at(const int64_t *sorted, long n, double p)
{
	long i = (long)(p / 100.0 * (double)(n - 1) + 0.5);
	return sorted[i < n ? i : n - 1];
}

static int cmd_wake(int argc, char **argv)
{
	long interval_us = 1000, count = 10000, delay_us = 0;
	int64_t *over;
	struct timespec next;
	int opt;
	optind = 2;
	while ((opt = getopt(argc, argv, "i:n:d:")) != -1) {
		switch (opt) {
		case 'i': interval_us = atol(optarg); break;
		case 'n': count = atol(optarg); break;
		case 'd': delay_us = atol(optarg); break;
		default: return 2;
		}
	}
	if (interval_us <= 0 || count <= 0 || delay_us < 0)
		return 2;
	over = calloc((size_t)count, sizeof *over);
	if (!over)
		return 1;
	clock_gettime(CLOCK_MONOTONIC, &next);
	for (long i = 0; i < count; i++) {
		int64_t target, woke;
		next.tv_nsec += interval_us * 1000;
		while (next.tv_nsec >= 1000000000) {
			next.tv_nsec -= 1000000000;
			next.tv_sec++;
		}
		while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL) == EINTR)
			;
		if (delay_us) {
			int64_t until = now_ns() + delay_us * 1000;
			while (now_ns() < until)
				;
		}
		woke = now_ns();
		target = (int64_t)next.tv_sec * 1000000000 + next.tv_nsec;
		over[i] = woke - target;
		/* Cyclictest's rule: a wake-up later than a whole period
		 * starts the schedule again from now instead of firing the
		 * missed ones back to back. */
		if (over[i] > interval_us * 1000) {
			next.tv_sec = woke / 1000000000;
			next.tv_nsec = woke % 1000000000;
		}
	}
	qsort(over, (size_t)count, sizeof *over, compare);
	printf("wake: %ld sleeps of %ld us to an absolute CLOCK_MONOTONIC time%s\n", count,
	       interval_us, delay_us ? " (with a spin after each)" : "");
	printf("wake: overshoot us min %.1f median %.1f p99 %.1f p99.9 %.1f p99.99 %.1f max %.1f\n",
	       over[0] / 1e3, at(over, count, 50) / 1e3, at(over, count, 99) / 1e3,
	       at(over, count, 99.9) / 1e3, at(over, count, 99.99) / 1e3,
	       over[count - 1] / 1e3);
	free(over);
	return 0;
}

/* MemTotal and MemAvailable from /proc/meminfo, in KiB. */
static int meminfo(long long *total, long long *available)
{
	FILE *f = fopen("/proc/meminfo", "r");
	char line[256];
	*total = *available = -1;
	if (!f)
		return -1;
	while (fgets(line, sizeof line, f)) {
		sscanf(line, "MemTotal: %lld", total);
		sscanf(line, "MemAvailable: %lld", available);
	}
	fclose(f);
	return *total > 0 && *available >= 0 ? 0 : -1;
}

static int cmd_hold(int argc, char **argv)
{
	int percent = argc > 2 ? atoi(argv[2]) : 80;
	const size_t chunk = 16u << 20;
	long long held = 0;
	int said = 0;
	if (percent <= 0 || percent >= 100)
		return 2;
	for (;;) {
		long long total, available;
		if (meminfo(&total, &available) != 0) {
			fprintf(stderr, "rig: /proc/meminfo has no MemTotal or MemAvailable\n");
			return 1;
		}
		if ((total - available) * 100 < total * percent) {
			char *p = mmap(NULL, chunk, PROT_READ | PROT_WRITE,
				       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
			if (p == MAP_FAILED) {
				sleep(1);
				continue;
			}
			/* Distinct bytes, so that no page is shared or
			 * compressed away. */
			for (size_t i = 0; i < chunk; i += 4096)
				memset(p + i, (int)(i >> 12) | 1, 4096);
			held += chunk >> 20;
			said = 0;
			continue;
		}
		if (!said) {
			printf("hold: %lld MiB held, %lld%% of %lld MiB in use\n", held,
			       (total - available) * 100 / total, total >> 10);
			fflush(stdout);
			said = 1;
		}
		sleep(1);
	}
}

static int cmd_write(int argc, char **argv)
{
	long mib = argc > 3 ? atol(argv[3]) : 1024;
	static char block[1 << 20];
	char path[4096];
	if (argc < 3 || mib <= 0)
		return 2;
	snprintf(path, sizeof path, "%s/rig-write.bin", argv[2]);
	for (size_t i = 0; i < sizeof block; i++)
		block[i] = (char)(i * 131 + 7);
	for (long pass = 1;; pass++) {
		int64_t start = now_ns();
		int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
		if (fd < 0) {
			perror("rig: write");
			return 1;
		}
		for (long i = 0; i < mib; i++) {
			if (write(fd, block, sizeof block) != (ssize_t)sizeof block) {
				perror("rig: write");
				return 1;
			}
		}
		fsync(fd);
		close(fd);
		printf("write: pass %ld, %ld MiB at %.1f MiB/s\n", pass, mib,
		       mib / ((now_ns() - start) / 1e9));
		fflush(stdout);
	}
}

static int cmd_launch(int argc, char **argv)
{
	int fds[2];
	char number[32], line[128];
	long long ctor = 0, swap = 0;
	int64_t t0;
	ssize_t got;
	size_t have = 0;
	pid_t child;
	int status;
	if (argc < 3)
		return 2;
	if (pipe(fds) != 0)
		return 1;
	snprintf(number, sizeof number, "%d", fds[1]);
	setenv("RIG_LAUNCH_FD", number, 1);
	t0 = now_ns();
	snprintf(number, sizeof number, "%lld", (long long)t0);
	setenv("RIG_LAUNCH_T0_NS", number, 1);
	child = fork();
	if (child < 0)
		return 1;
	if (child == 0) {
		close(fds[0]);
		execvp(argv[2], argv + 2);
		perror("rig: launch");
		_exit(127);
	}
	close(fds[1]);
	while (have < sizeof line - 1 &&
	       (got = read(fds[0], line + have, sizeof line - 1 - have)) > 0)
		have += (size_t)got;
	line[have] = 0;
	close(fds[0]);
	if (sscanf(line, "ctor %lld swap %lld", &ctor, &swap) == 2)
		printf("launch: constructor +%.3f ms, first swap returned +%.3f ms\n",
		       (ctor - t0) / 1e6, (swap - t0) / 1e6);
	else
		printf("launch: no first swap reported\n");
	fflush(stdout);
	if (waitpid(child, &status, 0) < 0)
		return 1;
	return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}

static int cmd_fake_game(int argc, char **argv)
{
	const char *fd = getenv("RIG_LAUNCH_FD");
	long ms = argc > 2 ? atol(argv[2]) : 0;
	long long ctor = now_ns(), swap;
	char line[128];
	struct timespec pause = { ms / 1000, (ms % 1000) * 1000000 };
	nanosleep(&pause, NULL);
	swap = now_ns();
	if (fd) {
		int n = snprintf(line, sizeof line, "ctor %lld swap %lld\n", ctor, swap);
		if (write(atoi(fd), line, (size_t)n) != n)
			return 1;
		close(atoi(fd));
	}
	return 0;
}

int main(int argc, char **argv)
{
	const char *c = argc > 1 ? argv[1] : "";
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (!strcmp(c, "cpus"))
		return cmd_cpus();
	if (!strcmp(c, "busy"))
		return cmd_busy(argc, argv);
	if (!strcmp(c, "wake"))
		return cmd_wake(argc, argv);
	if (!strcmp(c, "hold"))
		return cmd_hold(argc, argv);
	if (!strcmp(c, "write"))
		return cmd_write(argc, argv);
	if (!strcmp(c, "launch"))
		return cmd_launch(argc, argv);
	if (!strcmp(c, "fake-game"))
		return cmd_fake_game(argc, argv);
	fprintf(stderr, "usage: rig cpus | busy S | wake [-i US] [-n N] [-d US] | hold [PCT] | "
			"write DIR [MIB] | launch PROGRAM ARGS... | fake-game [MS]\n");
	return 2;
}
