// lbench: Linux IPC floors, timed the way Ferrix's ipc-bench and the seL4
// root task are (docs/OPAQUE-KERNEL.md §9.6): `lfence; rdtsc; lfence` around
// each sample, one sample = one operation (a null call or a full round
// trip), samples sorted, percentiles read off the sorted array.
//
// Runs as /init of an initramfs on a one-vCPU guest, so both sides of every
// ping-pong share the one processor: each round trip is two switches of
// address space, like Ferrix's domain-call and seL4's Call + ReplyRecv.
//
// Build: gcc -O2 -static -o init lbench.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <dirent.h>

#define WARMUP 2000
#define SAMPLES 20000
#define REPEATS 5

static uint64_t samples[SAMPLES];

static inline uint64_t tsc(void) {
    uint32_t lo, hi;
    __asm__ volatile("lfence; rdtsc; lfence" : "=a"(lo), "=d"(hi)::"memory");
    return ((uint64_t)hi << 32) | lo;
}

static int cmp(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static double mhz;

static void report(const char *name, int rep) {
    qsort(samples, SAMPLES, sizeof samples[0], cmp);
    uint64_t sum = 0;
    for (int i = 0; i < SAMPLES; i++) sum += samples[i];
    uint64_t p50 = samples[SAMPLES / 2], p90 = samples[SAMPLES * 9 / 10],
             p99 = samples[SAMPLES * 99 / 100];
    printf("LB name=%s rep=%d n=%d min=%llu p50=%llu p90=%llu p99=%llu mean=%llu "
           "p50_ns=%.0f p90_ns=%.0f p99_ns=%.0f\n",
           name, rep, SAMPLES, (unsigned long long)samples[0], (unsigned long long)p50,
           (unsigned long long)p90, (unsigned long long)p99,
           (unsigned long long)(sum / SAMPLES), p50 * 1000.0 / mhz, p90 * 1000.0 / mhz,
           p99 * 1000.0 / mhz);
    fflush(stdout);
}

static double calibrate(void) {
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC_RAW, &a);
    uint64_t t0 = tsc();
    do clock_gettime(CLOCK_MONOTONIC_RAW, &b);
    while ((b.tv_sec - a.tv_sec) * 1000000000LL + (b.tv_nsec - a.tv_nsec) < 300000000LL);
    uint64_t t1 = tsc();
    double ns = (b.tv_sec - a.tv_sec) * 1e9 + (b.tv_nsec - a.tv_nsec);
    return (t1 - t0) * 1000.0 / ns;
}

// The counter's own cost: two reads back to back.
static void bench_empty(void) {
    for (int rep = 1; rep <= REPEATS; rep++) {
        for (int i = 0; i < WARMUP + SAMPLES; i++) {
            uint64_t t0 = tsc();
            uint64_t t1 = tsc();
            if (i >= WARMUP) samples[i - WARMUP] = t1 - t0;
        }
        report("counter", rep);
    }
}

static void bench_null(void) {
    for (int rep = 1; rep <= REPEATS; rep++) {
        for (int i = 0; i < WARMUP + SAMPLES; i++) {
            uint64_t t0 = tsc();
            syscall(SYS_getppid);
            uint64_t t1 = tsc();
            if (i >= WARMUP) samples[i - WARMUP] = t1 - t0;
        }
        report("null-getppid", rep);
    }
}

static void die(const char *what) {
    printf("LB error %s: %s\n", what, strerror(errno));
    fflush(stdout);
    _exit(1);
}

static void xread(int fd, void *buf, size_t n) {
    if (read(fd, buf, n) != (ssize_t)n) die("read");
}
static void xwrite(int fd, const void *buf, size_t n) {
    if (write(fd, buf, n) != (ssize_t)n) die("write");
}

// A ping-pong over two fds per side: the parent writes `len` bytes to `out`
// and reads them back from `in`; the child echoes.
static void pingpong(const char *name, int pin, int pout, int cin, int cout,
                     int close1, int close2, size_t len) {
    char buf[64] = {0};
    int total = REPEATS * (WARMUP + SAMPLES);
    pid_t child = fork();
    if (child < 0) die("fork");
    if (child == 0) {
        if (close1 >= 0) close(close1);
        if (close2 >= 0) close(close2);
        for (int i = 0; i < total; i++) {
            xread(cin, buf, len);
            xwrite(cout, buf, len);
        }
        _exit(0);
    }
    for (int rep = 1; rep <= REPEATS; rep++) {
        for (int i = 0; i < WARMUP + SAMPLES; i++) {
            uint64_t t0 = tsc();
            xwrite(pout, buf, len);
            xread(pin, buf, len);
            uint64_t t1 = tsc();
            if (i >= WARMUP) samples[i - WARMUP] = t1 - t0;
        }
        report(name, rep);
    }
    waitpid(child, NULL, 0);
}

static void bench_pipe(size_t len, const char *name) {
    int to[2], from[2];
    if (pipe(to) || pipe(from)) die("pipe");
    // Parent writes to[1], reads from[0]; child reads to[0], writes from[1].
    pingpong(name, from[0], to[1], to[0], from[1], to[1], from[0], len);
    close(to[0]); close(to[1]); close(from[0]); close(from[1]);
}

static void bench_unix(int type, const char *name) {
    int sv[2];
    if (socketpair(AF_UNIX, type, 0, sv)) die("socketpair");
    pingpong(name, sv[0], sv[0], sv[1], sv[1], sv[0], -1, 1);
    close(sv[0]); close(sv[1]);
}

static long futex(volatile uint32_t *addr, int op, uint32_t val) {
    return syscall(SYS_futex, addr, op, val, NULL, NULL, 0);
}

// A futex ping-pong over a shared page (not FUTEX_PRIVATE: two processes).
// The word is 0 when it is the child's turn to wait, 1 when the parent has
// sent; each side always makes its wake and wait calls.
static void bench_futex(void) {
    volatile uint32_t *word = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                   MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (word == MAP_FAILED) die("mmap");
    *word = 0;
    int total = REPEATS * (WARMUP + SAMPLES);
    pid_t child = fork();
    if (child < 0) die("fork");
    if (child == 0) {
        for (int i = 0; i < total; i++) {
            while (__atomic_load_n(word, __ATOMIC_ACQUIRE) == 0) futex(word, FUTEX_WAIT, 0);
            __atomic_store_n(word, 0, __ATOMIC_RELEASE);
            futex(word, FUTEX_WAKE, 1);
        }
        _exit(0);
    }
    for (int rep = 1; rep <= REPEATS; rep++) {
        for (int i = 0; i < WARMUP + SAMPLES; i++) {
            uint64_t t0 = tsc();
            __atomic_store_n(word, 1, __ATOMIC_RELEASE);
            futex(word, FUTEX_WAKE, 1);
            while (__atomic_load_n(word, __ATOMIC_ACQUIRE) == 1) futex(word, FUTEX_WAIT, 1);
            uint64_t t1 = tsc();
            if (i >= WARMUP) samples[i - WARMUP] = t1 - t0;
        }
        report("futex-pingpong", rep);
    }
    waitpid(child, NULL, 0);
}

static void cat(const char *path, const char *label) {
    char buf[512];
    int fd = open(path, O_RDONLY);
    if (fd < 0) { printf("LBINFO %s: (none)\n", label); return; }
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n < 0) n = 0;
    buf[n] = 0;
    while (n > 0 && buf[n - 1] == '\n') buf[--n] = 0;
    printf("LBINFO %s: %s\n", label, buf);
}

static void info(void) {
    struct utsname u;
    uname(&u);
    printf("LBINFO uname: %s %s %s %s\n", u.sysname, u.release, u.version, u.machine);
    cat("/proc/cmdline", "cmdline");
    cat("/sys/devices/system/clocksource/clocksource0/current_clocksource", "clocksource");
    cat("/sys/kernel/debug/sched/preempt", "preempt");
    DIR *d = opendir("/sys/devices/system/cpu/vulnerabilities");
    if (d) {
        struct dirent *e;
        char path[300];
        while ((e = readdir(d))) {
            if (e->d_name[0] == '.') continue;
            snprintf(path, sizeof path, "/sys/devices/system/cpu/vulnerabilities/%s", e->d_name);
            char label[300];
            snprintf(label, sizeof label, "vuln %s", e->d_name);
            cat(path, label);
        }
        closedir(d);
    }
    FILE *f = fopen("/proc/cpuinfo", "r");
    if (f) {
        char line[4096];
        while (fgets(line, sizeof line, f))
            if (!strncmp(line, "model name", 10) || !strncmp(line, "flags", 5) ||
                !strncmp(line, "bugs", 4))
                printf("LBINFO cpuinfo %s", line);
        fclose(f);
    }
    printf("LBINFO cpus online: %ld\n", sysconf(_SC_NPROCESSORS_ONLN));
    fflush(stdout);
}

int main(void) {
    mkdir("/proc", 0555);
    mkdir("/sys", 0555);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("debugfs", "/sys/kernel/debug", "debugfs", 0, NULL);
    setvbuf(stdout, NULL, _IOLBF, 0);
    info();
    mhz = calibrate();
    printf("LBINFO tsc_mhz: %.1f\n", mhz);
    printf("LB start warmup=%d samples=%d repeats=%d\n", WARMUP, SAMPLES, REPEATS);
    bench_empty();
    bench_null();
    bench_pipe(1, "pipe-pingpong-1B");
    bench_pipe(8, "pipe-pingpong-8B");
    bench_futex();
    bench_unix(SOCK_STREAM, "unix-stream-pingpong-1B");
    bench_unix(SOCK_SEQPACKET, "unix-seqpacket-pingpong-1B");
    printf("LB done\n");
    fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    return 0;
}
