/* lbench: Linux's side of the DK1 IPC comparison (docs/BOARD-BENCH.md, B1).
 *
 * The ARMv7-A port of the x86-64 lbench (rdtsc, run as /init on one vCPU),
 * timed by board-bench.h as the seL4 bench and Ferrix's ipc-bench are:
 * PMCCNTR and instructions retired around every sample, WARMUP untimed and
 * then SAMPLES timed operations, percentiles off the sorted samples.
 *
 * It runs as /init of an initramfs:
 *   1. pins itself, and so every child, to CPU 0: both sides of a ping-pong
 *      share one processor, and each round trip is two switches of address
 *      space, as Ferrix's domain-call and seL4's Call + ReplyRecv are;
 *   2. loads /pmu-user.ko (PMUSERENR.EN on every CPU);
 *   3. prints, first: the clock line, the kernel version, the module's
 *      SCTLR/ACTLR/CNTKCTL line per CPU (read back from the kernel log) and
 *      whether PMUSERENR.EN is set; then the clock check (three measurements
 *      agree) and what else describes the run;
 *   4. runs the series and measures the clock again;
 *   5. prints `LB done` and `board-bench end lbench`, waits for the console
 *      to drain, and resets (reboot -f: PSCI SYSTEM_RESET), so a board
 *      returns to U-Boot with no hand at it. An error ends the same way,
 *      after an `lbench error` line.
 *
 * Series (<what> in board-bench.h's lines; the program name is `lbench`):
 *   timer-floor      the counter reads around an empty statement
 *   null             syscall(SYS_getppid)
 *   pipe, pipe-8B    write 1 (8) bytes to a pipe, read them back from a
 *                    second pipe; a child echoes
 *   unix-stream, unix-seqpacket
 *                    1 byte over an AF_UNIX socketpair, echoed
 *   futex            two processes, a word on a shared page (not private):
 *                    store 1 + wake, then wait until the child stores 0 + wakes
 *   base.wW          bb_touch(buf, W, 4096) alone, W = 1, 16, 64, 256 pages
 *   null.after.wW    the same touch, right after a null syscall
 *   pipe.after.wW    the same touch, right after a pipe round trip
 * In the last three only the touch is timed: after - base is what the call
 * leaves behind for the caller's own work (caches, TLB, predictors).
 *
 * Build (tools/common/bench/board/linux/build.sh does it):
 *   arm-linux-gnueabihf-gcc -O2 -static -marm -mcpu=cortex-a7 -I../common -o lbench lbench.c
 * -marm makes this file ARM code; the libc.a wrappers it calls (syscall,
 * read, write) are whatever mode the C library was built in.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

#include "board-bench.h"

#define PROG "lbench"
#define MODULE_PATH "/pmu-user.ko"
#define PAGE 4096u
#define MAX_W 256u
/* Three clock measurements may differ by this much (parts per million). */
#define CLOCK_PPM_MAX 1000u

static struct bb_series s;
/* The pages bb_touch walks. Each is written before use: an untouched BSS page
 * maps the shared zero page, and 256 loads from one frame would measure
 * nothing. */
static char buf[MAX_W * PAGE] __attribute__((aligned(PAGE)));
static const uint32_t widths[] = {1, 16, 64, 256};
static uint64_t hz;
/* The kernel log, read once for the module's lines (2^LOG_BUF_SHIFT, 17 in
 * multi_v7_defconfig). */
static char klog[1u << 17];

/* The end of every run, good or bad: the end line, the console drained, then
 * a reset (what `reboot -f` does). A child only exits. */
static __attribute__((noreturn)) void end_run(int status)
{
    if (getpid() != 1)
        _exit(status);
    printf("board-bench end %s\n", PROG);
    fflush(stdout);
    tcdrain(STDOUT_FILENO);
    sync();
    reboot(RB_AUTOBOOT);
    _exit(status);
}

static __attribute__((noreturn)) void die(const char *what)
{
    printf("%s error %s: %s\n", PROG, what, strerror(errno));
    end_run(1);
}

static void pin0(void)
{
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(0, &set);
    if (sched_setaffinity(0, sizeof set, &set) != 0)
        die("sched_setaffinity");
}

static void xread(int fd, void *p, size_t n)
{
    if (read(fd, p, n) != (ssize_t)n)
        die("read");
}

static void xwrite(int fd, const void *p, size_t n)
{
    if (write(fd, p, n) != (ssize_t)n)
        die("write");
}

static pid_t xfork(void)
{
    pid_t pid = fork();
    if (pid < 0)
        die("fork");
    if (pid == 0)
        pin0();
    return pid;
}

static void reap(pid_t child)
{
    int st;
    if (waitpid(child, &st, 0) != child)
        die("waitpid");
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        errno = ECHILD;
        die("child");
    }
}

/* ---- ping-pong over two fds: the parent writes `len` bytes to `out` and
 * reads them back from `in`; a child echoes `count` messages. */

struct pp {
    int out, in;
    size_t len;
    pid_t child;
    int close_a, close_b; /* the parent's fds, closed when the child is reaped */
};

static char msg[64];

static __attribute__((noreturn)) void pp_echo(int cin, int cout, size_t len, uint32_t count)
{
    char m[64];
    for (uint32_t i = 0; i < count; i++) {
        xread(cin, m, len);
        xwrite(cout, m, len);
    }
    _exit(0);
}

static void pp_start_pipe(struct pp *p, size_t len, uint32_t count)
{
    int to[2], from[2];
    if (pipe(to) || pipe(from))
        die("pipe");
    p->child = xfork();
    if (p->child == 0) {
        close(to[1]);
        close(from[0]);
        pp_echo(to[0], from[1], len, count);
    }
    /* Parent: writes to[1], reads from[0]. Closing the child's ends makes a
     * dead child an EOF rather than a hang. */
    close(to[0]);
    close(from[1]);
    p->out = to[1];
    p->in = from[0];
    p->len = len;
    p->close_a = to[1];
    p->close_b = from[0];
}

static void pp_start_unix(struct pp *p, int type, uint32_t count)
{
    int sv[2];
    if (socketpair(AF_UNIX, type, 0, sv))
        die("socketpair");
    p->child = xfork();
    if (p->child == 0) {
        close(sv[0]);
        pp_echo(sv[1], sv[1], 1, count);
    }
    close(sv[1]);
    p->out = p->in = sv[0];
    p->len = 1;
    p->close_a = sv[0];
    p->close_b = -1;
}

static void pp_stop(struct pp *p)
{
    reap(p->child);
    close(p->close_a);
    if (p->close_b >= 0)
        close(p->close_b);
}

static struct pp cur; /* the ping-pong the round trip below uses */

static inline void pp_rt(void)
{
    xwrite(cur.out, msg, cur.len);
    xread(cur.in, msg, cur.len);
}

/* ---- futex ping-pong, as the x86 lbench: the word is 0 when it is the
 * child's turn to wait, 1 when the parent has sent; each side always makes
 * its wake and wait calls. */

static volatile uint32_t *word;

static long futex(volatile uint32_t *addr, int op, uint32_t val)
{
    return syscall(SYS_futex, addr, op, val, NULL, NULL, 0);
}

static inline void futex_rt(void)
{
    __atomic_store_n(word, 1, __ATOMIC_RELEASE);
    futex(word, FUTEX_WAKE, 1);
    while (__atomic_load_n(word, __ATOMIC_ACQUIRE) == 1)
        futex(word, FUTEX_WAIT, 1);
}

static void bench_futex(void)
{
    const uint32_t count = BB_WARMUP + BB_SAMPLES;
    word = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (word == MAP_FAILED)
        die("mmap");
    *word = 0;
    pid_t child = xfork();
    if (child == 0) {
        for (uint32_t i = 0; i < count; i++) {
            while (__atomic_load_n(word, __ATOMIC_ACQUIRE) == 0)
                futex(word, FUTEX_WAIT, 0);
            __atomic_store_n(word, 0, __ATOMIC_RELEASE);
            futex(word, FUTEX_WAKE, 1);
        }
        _exit(0);
    }
    BB_RUN(&s, futex_rt());
    reap(child);
    munmap((void *)word, PAGE);
    bb_print(PROG, "futex", &s, hz);
}

/* ---- the round trips */

static void bench_pipe(size_t len, const char *what)
{
    pp_start_pipe(&cur, len, BB_WARMUP + BB_SAMPLES);
    BB_RUN(&s, pp_rt());
    pp_stop(&cur);
    bb_print(PROG, what, &s, hz);
}

static void bench_unix(int type, const char *what)
{
    pp_start_unix(&cur, type, BB_WARMUP + BB_SAMPLES);
    BB_RUN(&s, pp_rt());
    pp_stop(&cur);
    bb_print(PROG, what, &s, hz);
}

/* ---- the cost after the call: `call` (untimed, none for the base), then
 * the touch of `w` pages, timed alone. */

static void null_call(void)
{
    syscall(SYS_getppid);
}

static void after_series(const char *what, void (*call)(void), uint32_t w)
{
    char name[48];
    for (uint32_t i = 0; i < BB_WARMUP; i++) {
        if (call)
            call();
        bb_touch(buf, w, PAGE);
    }
    bb_reset(&s);
    for (uint32_t i = 0; i < BB_SAMPLES; i++) {
        if (call)
            call();
        BB_TIME(&s, bb_touch(buf, w, PAGE));
    }
    snprintf(name, sizeof name, "%s.w%lu", what, (unsigned long)w);
    bb_print(PROG, name, &s, hz);
}

static void bench_after(void)
{
    for (size_t i = 0; i < sizeof widths / sizeof widths[0]; i++) {
        uint32_t w = widths[i];
        after_series("base", NULL, w);
        after_series("null.after", null_call, w);
        pp_start_pipe(&cur, 1, BB_WARMUP + BB_SAMPLES);
        after_series("pipe.after", pp_rt, w);
        pp_stop(&cur);
    }
}

/* ---- setup and description */

static void load_module(void)
{
    int fd = open(MODULE_PATH, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        die("open " MODULE_PATH);
    if (syscall(SYS_finit_module, fd, "", 0) != 0 && errno != EEXIST)
        die("finit_module " MODULE_PATH);
    close(fd);
}

/* The module's per-CPU lines (SCTLR, ACTLR, CNTKCTL, PMUSERENR), from the
 * kernel log, so they are in this program's output whatever the console's
 * order. */
static void print_module_lines(void)
{
    int n = klogctl(3 /* SYSLOG_ACTION_READ_ALL */, klog, sizeof klog - 1);
    int found = 0;
    if (n < 0)
        n = 0;
    klog[n] = 0;
    for (char *line = klog, *next; line && *line; line = next) {
        next = strchr(line, '\n');
        if (next)
            *next++ = 0;
        char *p = strstr(line, "pmu-user: ");
        if (p) {
            printf("%s info %s\n", PROG, p);
            found = 1;
        }
    }
    if (!found)
        printf("%s info pmu-user: (no line in the kernel log)\n", PROG);
}

static void cat(const char *path, const char *label)
{
    char b[512];
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        printf("%s info %s: (none)\n", PROG, label);
        return;
    }
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    if (n < 0)
        n = 0;
    b[n] = 0;
    while (n > 0 && b[n - 1] == '\n')
        b[--n] = 0;
    printf("%s info %s: %s\n", PROG, label, b);
}

/* The lines of a file that start with one of `keys`. */
static void grep_lines(const char *path, const char *label, const char *const *keys)
{
    char line[1024];
    FILE *f = fopen(path, "r");
    if (!f) {
        printf("%s info %s: (none)\n", PROG, label);
        return;
    }
    while (fgets(line, sizeof line, f))
        for (const char *const *k = keys; *k; k++)
            if (!strncmp(line, *k, strlen(*k))) {
                printf("%s info %s %s", PROG, label, line);
                break;
            }
    fclose(f);
}

/* The memory the kernel was given: top-level /proc/iomem ranges of RAM, and
 * which reserved-memory nodes the device tree carried (OP-TEE's among them
 * on the board, if U-Boot or the boot lines added it). */
static void memory_info(void)
{
    char line[256];
    FILE *f = fopen("/proc/iomem", "r");
    if (f) {
        while (fgets(line, sizeof line, f))
            if (line[0] != ' ' && (strstr(line, "System RAM") || strstr(line, "reserved")))
                printf("%s info iomem %s", PROG, line);
        fclose(f);
    }
    static const char *const meminfo[] = {"MemTotal", NULL};
    grep_lines("/proc/meminfo", "meminfo", meminfo);
    printf("%s info reserved-memory:", PROG);
    DIR *d = opendir("/sys/firmware/devicetree/base/reserved-memory");
    if (d) {
        struct dirent *e;
        while ((e = readdir(d)))
            if (e->d_name[0] != '.' && strchr(e->d_name, '@'))
                printf(" %s", e->d_name);
        closedir(d);
    } else {
        printf(" (none)");
    }
    printf("\n");
}

static void info(void)
{
    cat("/proc/cmdline", "cmdline");
    printf("%s info cpus online: %ld\n", PROG, sysconf(_SC_NPROCESSORS_ONLN));
    cat("/sys/devices/system/clocksource/clocksource0/current_clocksource", "clocksource");
    cat("/sys/kernel/debug/sched/preempt", "preempt");
    memory_info();
    DIR *d = opendir("/sys/devices/system/cpu/vulnerabilities");
    if (d) {
        struct dirent *e;
        char path[320], label[300];
        while ((e = readdir(d))) {
            if (e->d_name[0] == '.')
                continue;
            snprintf(path, sizeof path, "/sys/devices/system/cpu/vulnerabilities/%s", e->d_name);
            snprintf(label, sizeof label, "vuln %s", e->d_name);
            cat(path, label);
        }
        closedir(d);
    }
    static const char *const cpuinfo[] = {"processor", "model name", "Features", "CPU part",
                                          "CPU revision", NULL};
    grep_lines("/proc/cpuinfo", "cpuinfo", cpuinfo);
#ifdef __thumb2__
    printf("%s info isa: thumb2\n", PROG);
#else
    printf("%s info isa: arm\n", PROG);
#endif
    printf("%s info compiler: gcc %s\n", PROG, __VERSION__);
    printf("%s info contract: warmup=%u samples=%u\n", PROG, BB_WARMUP, BB_SAMPLES);
}

static uint64_t ppm(uint64_t a, uint64_t b, uint64_t ref)
{
    uint64_t d = a > b ? a - b : b - a;
    return ref ? d * 1000000u / ref : UINT64_MAX;
}

/* Three measurements of the clock, sorted; the median is the clock line's. */
static void measure_clock(uint64_t m[3])
{
    for (int i = 0; i < 3; i++)
        m[i] = bb_cpu_hz();
    for (int i = 0; i < 2; i++)
        for (int j = 0; j < 2 - i; j++)
            if (m[j] > m[j + 1]) {
                uint64_t t = m[j];
                m[j] = m[j + 1];
                m[j + 1] = t;
            }
}

int main(void)
{
    mkdir("/proc", 0555);
    mkdir("/sys", 0555);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("debugfs", "/sys/kernel/debug", "debugfs", 0, NULL);
    setvbuf(stdout, NULL, _IOLBF, 0);

    pin0();
    load_module();

    /* The first lines: clock, kernel, SCTLR/ACTLR, PMUSERENR.EN. */
    uint32_t pmuserenr = bb_pmuserenr();
    int pmu = bb_pmu_start() == 0;
    uint64_t m[3] = {0, 0, 0};
    if (pmu) {
        measure_clock(m);
        hz = m[1];
        bb_print_clock(PROG, hz);
    }
    struct utsname u;
    uname(&u);
    printf("%s info kernel: %s %s %s %s\n", PROG, u.sysname, u.release, u.version, u.machine);
    print_module_lines();
    printf("%s info pmuserenr: 0x%08lx en=%lu\n", PROG, (unsigned long)pmuserenr,
           (unsigned long)(pmuserenr & 1u));
    if (!pmu) {
        errno = EPERM;
        die("pmu-user-access-off");
    }
    uint64_t spread = ppm(m[2], m[0], m[1]);
    printf("%s clock-check hz=%llu,%llu,%llu spread_ppm=%llu ok=%d\n", PROG,
           (unsigned long long)m[0], (unsigned long long)m[1], (unsigned long long)m[2],
           (unsigned long long)spread, spread <= CLOCK_PPM_MAX);
    if (bb_cntfrq() == 0 || hz == 0) {
        errno = EINVAL;
        die("clock");
    }
    info();

    memset(buf, 0xa5, sizeof buf);

    BB_RUN(&s, (void)0);
    bb_print(PROG, "timer-floor", &s, hz);
    BB_RUN(&s, syscall(SYS_getppid));
    bb_print(PROG, "null", &s, hz);
    bench_pipe(1, "pipe");
    bench_pipe(8, "pipe-8B");
    bench_unix(SOCK_STREAM, "unix-stream");
    bench_unix(SOCK_SEQPACKET, "unix-seqpacket");
    bench_futex();
    bench_after();

    uint64_t end = bb_cpu_hz();
    uint64_t drift = ppm(end, hz, hz);
    printf("%s clock-end cpu_hz=%llu drift_ppm=%llu ok=%d\n", PROG, (unsigned long long)end,
           (unsigned long long)drift, drift <= CLOCK_PPM_MAX);
    printf("LB done\n");
    end_run(0);
}
