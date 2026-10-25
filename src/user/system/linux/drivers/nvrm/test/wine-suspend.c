/*
 * wine-suspend: Wine's SuspendThread and GetThreadContext against threads
 * in render node ioctls, with raw DRM calls and no Mesa.
 *
 * Wine suspends a thread by sending it SIGUSR1 (wineserver's tkill); the
 * thread's handler reports its context to the server and then blocks in a
 * read on its wait pipe until it is resumed, and only then returns, its
 * interrupted call restarted (SA_RESTART). Unity's collector suspends every
 * thread of the game this way, many times a second while it loads, and
 * gives up ("SuspendThread loop failed") when a thread never reports.
 *
 * Workers, each on its own descriptor of the render node:
 *   - fast: GET_CAP(DRM_CAP_SYNCOBJ), SYNCOBJ_CREATE and DESTROY, over and
 *     over; every answer checked;
 *   - wait: SYNCOBJ_WAIT with WAIT_FOR_SUBMIT on a syncobj nothing signals,
 *     to an absolute deadline 300 us out, held in the driver: every call
 *     must end in ETIME, never EINTR (Linux's jiffies may end it a little
 *     early, so an early end is counted, not failed);
 *   - mixed: both, alternately;
 *   - futex: FUTEX_WAIT with a 200 us timeout on a word nothing wakes,
 *     ETIMEDOUT or, as on Linux, EINTR after a handler;
 *   - spin: no system call at all;
 *   - masked: Wine's server call, SIGUSR1 blocked around a write and a
 *     read on its own pipe, then 50 us of user code;
 *   - waitv: futex_waitv to an absolute time 200 us out, as DW-Proton's
 *     in-process sync waits: ETIMEDOUT, restarted under SA_RESTART;
 *   - pselect: pselect6 for 200 us on a pipe nothing writes: a timeout,
 *     or EINTR after a handler as on Linux.
 * The suspender, as wineserver does, tgkills a worker with SIGUSR1 and
 * waits for its handler's report over a pipe, then resumes it through the
 * worker's own pipe; in every fourth round it suspends all of them first
 * and resumes them after, as the collector does. A report later than 2 s
 * is a lost suspend, said with the thread's /proc state.
 *
 * The handler checks the context it was given (x86-64): never a kernel
 * restart code (-512 to -516) in rax, and a call rewound to its syscall
 * instruction for a restart holds the call's own number and descriptor.
 *
 * Prints "wine-suspend: R rounds, S suspends, L lost, B bad contexts,
 * E errors: PASS|FAIL" and exits 0 only on PASS. Built statically on the
 * host; runs the same on Linux, against any render node with syncobjs.
 *
 * SPDX-License-Identifier: MIT
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

struct drm_get_cap { uint64_t capability, value; };
struct drm_syncobj_create { uint32_t handle, flags; };
struct drm_syncobj_destroy { uint32_t handle, pad; };
struct drm_syncobj_wait {
    uint64_t handles;
    int64_t timeout_nsec;
    uint32_t count_handles, flags, first_signaled, pad;
    uint64_t deadline_nsec;
};
#define IOWR(nr, size) ((3u << 30) | ((uint32_t)(size) << 16) | ('d' << 8) | (nr))
#define GET_CAP IOWR(0x0c, sizeof(struct drm_get_cap))
#define SYNCOBJ_CREATE IOWR(0xbf, sizeof(struct drm_syncobj_create))
#define SYNCOBJ_DESTROY IOWR(0xc0, sizeof(struct drm_syncobj_destroy))
#define SYNCOBJ_WAIT IOWR(0xc3, sizeof(struct drm_syncobj_wait))
#define CAP_SYNCOBJ 0x13
#define WAIT_FOR_SUBMIT (1u << 1)
#define WAIT_NS 300000
#define REPORT_MS 2000
#define LATE_MS 20000

enum kind { FAST, WAIT, MIXED, FUTEX, SPIN, MASKED, WAITV, PSELECT, KINDS };
static const char *const kind_name[KINDS] = { "fast", "wait", "mixed", "futex", "spin",
                                              "masked", "waitv", "pselect" };

struct futex_waitv_entry { uint64_t val, uaddr; uint32_t flags, reserved; };
#ifndef SYS_futex_waitv
#define SYS_futex_waitv 449
#endif

struct worker {
    enum kind kind;
    int index, fd;
    pid_t tid;
    int wake[2];           /* the suspender writes a byte to resume it */
    int echo[2];           /* a masked worker's own pipe, its "server call" */
    pthread_t thread;
    volatile int in_call;  /* the call it is in: its syscall number, or 0 */
    volatile long calls, errors, eintr, early, restarts, suspended, lost;
    volatile long bad_code, bad_restart;
    volatile int last_errno;
    volatile int dead;     /* lost for good: no more rounds for it */
};

struct report { int index; long rip, rax; };

#define MAX_WORKERS 16
static struct worker workers[MAX_WORKERS];
static int nworkers;
static int report_pipe[2];
static volatile int stop;
static __thread struct worker *self;
static int futex_word;

static long now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000000L + ts.tv_nsec;
}

static void on_usr1(int sig, siginfo_t *info, void *context)
{
    (void)sig;
    (void)info;
    struct worker *w = self;
    if (!w)
        return;
    int saved = errno;
    struct report r = { w->index, 0, 0 };
#ifdef __x86_64__
    ucontext_t *uc = context;
    long rip = uc->uc_mcontext.gregs[REG_RIP];
    long rax = uc->uc_mcontext.gregs[REG_RAX];
    long rdi = uc->uc_mcontext.gregs[REG_RDI];
    r.rip = rip;
    r.rax = rax;
    if (rax <= -512 && rax >= -516)
        w->bad_code++;
    const unsigned char *at = (const unsigned char *)rip;
    if (w->in_call && at[0] == 0x0f && at[1] == 0x05) {
        /* Rewound to restart: the call's own number and first argument. */
        int fd_arg = w->in_call == SYS_ioctl ? w->fd : (int)rdi;
        if (rax != w->in_call || (int)rdi != fd_arg)
            w->bad_restart++;
        else
            w->restarts++;
    }
#else
    (void)context;
#endif
    w->suspended++;
    /* What Wine's wait_suspend does: tell the server, then wait. */
    if (write(report_pipe[1], &r, sizeof r) != (ssize_t)sizeof r)
        w->errors++;
    char byte;
    while (read(w->wake[0], &byte, 1) < 0 && errno == EINTR)
        ;
    errno = saved;
}

static void note_error(struct worker *w, int error)
{
    w->errors++;
    w->last_errno = error;
    if (error == EINTR)
        w->eintr++;
    if (stop)
        return;
    if (w->errors <= 5)
        printf("wine-suspend: %s worker %d: errno %d (%s)\n", kind_name[w->kind], w->index,
               error, strerror(error));
}

static void fast_call(struct worker *w)
{
    struct drm_get_cap cap = { CAP_SYNCOBJ, 0 };
    w->in_call = SYS_ioctl;
    int ret = ioctl(w->fd, GET_CAP, &cap);
    w->in_call = 0;
    w->calls++;
    if (ret != 0)
        note_error(w, errno);
    else if (cap.value != 1)
        note_error(w, 1000 + (int)cap.value);
    struct drm_syncobj_create create = { 0, 0 };
    w->in_call = SYS_ioctl;
    ret = ioctl(w->fd, SYNCOBJ_CREATE, &create);
    w->in_call = 0;
    w->calls++;
    if (ret != 0 || create.handle == 0) {
        note_error(w, ret ? errno : 2000);
        return;
    }
    struct drm_syncobj_destroy destroy = { create.handle, 0 };
    w->in_call = SYS_ioctl;
    ret = ioctl(w->fd, SYNCOBJ_DESTROY, &destroy);
    w->in_call = 0;
    w->calls++;
    if (ret != 0)
        note_error(w, errno);
}

static void wait_call(struct worker *w, uint32_t handle)
{
    long deadline = now_ns() + WAIT_NS;
    struct drm_syncobj_wait wait = {
        .handles = (uintptr_t)&handle, .timeout_nsec = deadline,
        .count_handles = 1, .flags = WAIT_FOR_SUBMIT,
    };
    w->in_call = SYS_ioctl;
    int ret = ioctl(w->fd, SYNCOBJ_WAIT, &wait);
    int error = errno;
    w->in_call = 0;
    w->calls++;
    if (ret == 0)
        note_error(w, 3000);
    else if (error != ETIME)
        note_error(w, error);
    else if (now_ns() < deadline)
        w->early++;
}

static void futex_call(struct worker *w)
{
    struct timespec ts = { 0, 200000 };
    w->in_call = SYS_futex;
    long ret = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, &ts, NULL, 0);
    int error = errno;
    w->in_call = 0;
    w->calls++;
    /* Linux ends a timed FUTEX_WAIT with EINTR on a handler whatever
     * SA_RESTART says (restart_block, ERESTART_RESTARTBLOCK). */
    if (ret == 0 || (error != ETIMEDOUT && error != EINTR))
        note_error(w, ret == 0 ? 4000 : error);
    else if (error == EINTR)
        w->eintr++;
}

/* Wine's server call: SIGUSR1 blocked (server_enter_uninterrupted_section),
 * a request written and its reply read, the mask put back -- a signal sent
 * meanwhile is delivered as the mask opens -- then game code, in user mode
 * with no call, for 50 us. */
static void masked_call(struct worker *w)
{
    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &block, &old);
    char byte = 1;
    w->in_call = SYS_write;
    if (write(w->echo[1], &byte, 1) != 1)
        note_error(w, errno);
    w->in_call = SYS_read;
    if (read(w->echo[0], &byte, 1) != 1)
        note_error(w, errno);
    w->in_call = 0;
    pthread_sigmask(SIG_SETMASK, &old, NULL);
    w->calls++;
    long until = now_ns() + 50000;
    while (now_ns() < until)
        ;
}

/* DW-Proton's in-process sync waits in futex_waitv to an absolute time. */
static void waitv_call(struct worker *w)
{
    struct futex_waitv_entry entry = { 0, (uintptr_t)&futex_word, 2 | 128, 0 };
    long deadline = now_ns() + 200000;
    struct timespec ts = { deadline / 1000000000L, deadline % 1000000000L };
    w->in_call = SYS_futex_waitv;
    long ret = syscall(SYS_futex_waitv, &entry, 1, 0, &ts, CLOCK_MONOTONIC);
    int error = errno;
    w->in_call = 0;
    w->calls++;
    if (ret >= 0 || error != ETIMEDOUT)
        note_error(w, ret >= 0 ? 5000 : error);
}

/* wineserver's and Chrome's threads in pselect6: EINTR after a handler on
 * Linux, whatever SA_RESTART says (ERESTARTNOHAND). */
static void pselect_call(struct worker *w)
{
    fd_set read_set;
    FD_ZERO(&read_set);
    FD_SET(w->echo[0], &read_set);
    struct timespec ts = { 0, 200000 };
    w->in_call = SYS_pselect6;
    int ret = pselect(w->echo[0] + 1, &read_set, NULL, NULL, &ts, NULL);
    int error = errno;
    w->in_call = 0;
    w->calls++;
    if (ret != 0 && error == EINTR)
        w->eintr++;
    else if (ret != 0)
        note_error(w, ret > 0 ? 6000 : error);
}

static void *work(void *arg)
{
    struct worker *w = arg;
    self = w;
    w->tid = (pid_t)syscall(SYS_gettid);
    uint32_t handle = 0;
    if (w->kind == WAIT || w->kind == MIXED) {
        struct drm_syncobj_create create = { 0, 0 };
        if (ioctl(w->fd, SYNCOBJ_CREATE, &create) != 0) {
            printf("wine-suspend: worker %d: SYNCOBJ_CREATE: %s\n", w->index, strerror(errno));
            w->errors++;
            return NULL;
        }
        handle = create.handle;
    }
    volatile unsigned long spins = 0;
    while (!stop) {
        switch (w->kind) {
        case FAST: fast_call(w); break;
        case WAIT: wait_call(w, handle); break;
        case MIXED: fast_call(w); wait_call(w, handle); break;
        case FUTEX: futex_call(w); break;
        case SPIN: for (int i = 0; i < 1000; i++) spins++; w->calls++; break;
        case MASKED: masked_call(w); break;
        case WAITV: waitv_call(w); break;
        case PSELECT: pselect_call(w); break;
        default: break;
        }
    }
    return NULL;
}

static void say_thread(struct worker *w)
{
    char path[96], text[512];
    const char *files[] = { "syscall", "wchan", "status" };
    for (unsigned i = 0; i < sizeof files / sizeof files[0]; i++) {
        snprintf(path, sizeof path, "/proc/self/task/%d/%s", w->tid, files[i]);
        int fd = open(path, O_RDONLY);
        if (fd < 0)
            continue;
        ssize_t got = read(fd, text, sizeof text - 1);
        close(fd);
        if (got <= 0)
            continue;
        text[got] = 0;
        for (char *line = strtok(text, "\n"); line; line = strtok(NULL, "\n"))
            if (i != 2 || !strncmp(line, "State", 5) || !strncmp(line, "SigPnd", 6) ||
                !strncmp(line, "ShdPnd", 6) || !strncmp(line, "SigBlk", 6))
                printf("wine-suspend:   %s: %s\n", files[i], line);
    }
    printf("wine-suspend:   in call %d, %ld calls, last errno %d\n", w->in_call, w->calls,
           w->last_errno);
}

/* Wait for `want` reports, flagging each worker that reported; false when
 * some did not within REPORT_MS (each then waited for up to LATE_MS more). */
static long lost_total, late_total, latency_max, latency_sum, suspends;
static int collect(int *reported, int want, long sent)
{
    int got = 0;
    long deadline = sent + REPORT_MS * 1000000L;
    int warned = 0;
    while (got < want) {
        long left = (deadline - now_ns()) / 1000000L;
        if (left < 0)
            left = 0;
        struct pollfd p = { report_pipe[0], POLLIN, 0 };
        int ready = poll(&p, 1, (int)left);
        if (ready > 0) {
            struct report r;
            if (read(report_pipe[0], &r, sizeof r) != (ssize_t)sizeof r)
                continue;
            if (r.index < 0 || r.index >= nworkers || reported[r.index])
                continue;
            reported[r.index] = 1;
            got++;
            long latency = now_ns() - sent;
            latency_sum += latency;
            if (latency > latency_max)
                latency_max = latency;
            if (warned) {
                late_total++;
                printf("wine-suspend: worker %d reported after %ld ms\n", r.index,
                       latency / 1000000L);
            }
            continue;
        }
        if (ready < 0 && errno == EINTR)
            continue;
        if (!warned) {
            warned = 1;
            for (int i = 0; i < nworkers; i++)
                if (!reported[i] && !workers[i].dead) {
                    printf("wine-suspend: %s worker %d (tid %d) did not report in %d ms\n",
                           kind_name[workers[i].kind], i, workers[i].tid, REPORT_MS);
                    say_thread(&workers[i]);
                }
            deadline = now_ns() + LATE_MS * 1000000L;
            continue;
        }
        for (int i = 0; i < nworkers; i++)
            if (!reported[i] && !workers[i].dead) {
                workers[i].lost++;
                workers[i].dead = 1;
                lost_total++;
                printf("wine-suspend: worker %d lost: no report in %d ms\n", i, LATE_MS);
            }
        return 0;
    }
    return 1;
}

static void resume(struct worker *w)
{
    char byte = 1;
    if (write(w->wake[1], &byte, 1) != 1)
        w->errors++;
}

int main(int argc, char **argv)
{
    int seconds = argc > 1 ? atoi(argv[1]) : 30;
    const char *node = argc > 2 ? argv[2] : "/dev/dri/renderD128";
    static const enum kind plan[] = { FAST, FAST, WAIT, MIXED, MIXED, FUTEX, SPIN, MASKED,
                                      WAITV, PSELECT };
    setvbuf(stdout, NULL, _IOLBF, 0);

    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = on_usr1;
    action.sa_flags = SA_SIGINFO | SA_RESTART;
    /* Wine's server_block_set, near enough: nothing nests in the handler. */
    sigfillset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0 || pipe(report_pipe) != 0) {
        perror("wine-suspend: setting up");
        return 1;
    }
    nworkers = (int)(sizeof plan / sizeof plan[0]);
    for (int i = 0; i < nworkers; i++) {
        struct worker *w = &workers[i];
        w->kind = plan[i];
        w->index = i;
        w->fd = open(node, O_RDWR | O_CLOEXEC);
        if (w->fd < 0 || pipe(w->wake) != 0 || pipe(w->echo) != 0) {
            printf("wine-suspend: opening %s: %s\n", node, strerror(errno));
            return 1;
        }
        if (pthread_create(&w->thread, NULL, work, w) != 0) {
            printf("wine-suspend: pthread_create failed\n");
            return 1;
        }
    }
    for (int i = 0; i < nworkers; i++)
        while (!workers[i].tid)
            usleep(1000);

    long end = now_ns() + seconds * 1000000000L;
    long rounds = 0;
    unsigned seed = 0x2545f491u;
    pid_t pid = getpid();
    while (now_ns() < end) {
        int reported[MAX_WORKERS] = { 0 };
        int all = rounds % 4 == 3;
        int want = 0;
        long sent = now_ns();
        if (all) {
            for (int i = 0; i < nworkers; i++) {
                if (workers[i].dead) {
                    reported[i] = 1;
                    continue;
                }
                syscall(SYS_tgkill, pid, workers[i].tid, SIGUSR1);
                want++;
            }
        } else {
            int i = (int)(rounds % (long)nworkers);
            for (int j = 0; j < nworkers; j++)
                reported[j] = j != i || workers[j].dead;
            if (!workers[i].dead) {
                syscall(SYS_tgkill, pid, workers[i].tid, SIGUSR1);
                want = 1;
            }
        }
        suspends += want;
        int complete = collect(reported, want, sent);
        (void)complete;
        for (int i = 0; i < nworkers; i++)
            if (!workers[i].dead && (all || (int)(rounds % (long)nworkers) == i))
                resume(&workers[i]);
        rounds++;
        /* A gap of 0 to 100 us between rounds, so a signal lands anywhere
         * in a call. */
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        long gap = (long)(seed % 100000u);
        long until = now_ns() + gap;
        if (seed & 1)
            while (now_ns() < until)
                ;
        else {
            struct timespec ts = { 0, gap };
            nanosleep(&ts, NULL);
        }
    }
    stop = 1;
    for (int i = 0; i < nworkers; i++)
        resume(&workers[i]);
    for (int i = 0; i < nworkers; i++)
        if (!workers[i].dead)
            pthread_join(workers[i].thread, NULL);

    long errors = 0, bad = 0, eintr = 0, early = 0;
    for (int i = 0; i < nworkers; i++) {
        struct worker *w = &workers[i];
        printf("wine-suspend: %s worker %d: %ld calls, %ld suspended, %ld restarted, %ld errors "
               "(%ld EINTR), %ld early, %ld bad codes, %ld bad restarts%s\n",
               kind_name[w->kind], i, w->calls, w->suspended, w->restarts, w->errors, w->eintr,
               w->early, w->bad_code, w->bad_restart, w->dead ? ", LOST" : "");
        errors += w->errors;
        bad += w->bad_code + w->bad_restart;
        if (w->kind != FUTEX && w->kind != PSELECT)
            eintr += w->eintr;
        early += w->early;
    }
    int pass = lost_total == 0 && late_total == 0 && bad == 0 && errors == 0;
    printf("wine-suspend: %ld rounds, %ld suspends (mean %ld us, max %ld us), %ld lost, %ld late, "
           "%ld bad contexts, %ld errors (%ld EINTR), %ld early: %s\n",
           rounds, suspends, suspends ? latency_sum / suspends / 1000 : 0, latency_max / 1000,
           lost_total, late_total, bad, errors, eintr, early, pass ? "PASS" : "FAIL");
    fflush(stdout);
    /* A lost worker still blocks in the kernel: leave without it. */
    _exit(pass ? 0 : 1);
}
