/*
 * nvrm's device files: the dispatcher for the requests the kernel's chardev
 * core forwards (docs/NVIDIA.md §4.4, N1e).
 *
 * nvrm says HELLO with /dev/nvidiactl and one /dev/nvidia<N> per GPU it
 * started, then takes REQUESTs one by one:
 *
 *  - OPEN opens the control device or a GPU (os/kept/nv.c) and remembers
 *    the file by the identity the kernel gave it;
 *  - IOCTL runs on a thread of its own, since RM may sleep, as the client
 *    the request names: its copy_in and copy_out are the bridge's, so RM's
 *    os_memcpy_*_user reach the program waiting in the call;
 *  - MMAP maps what NV_ESC_RM_MAP_MEMORY left on the file, as NVIDIA's
 *    nvidia_mmap_helper decides it: a GPU file's range of the card's BARs,
 *    uncached for registers and USERD and as RM asked for the framebuffer;
 *    the control file's system memory, the VMO the allocation is. The
 *    kernel checks the answer against the device and the VMO's rights;
 *  - RELEASE closes the file. The kernel sends one for a file it may never
 *    have heard back about, so an unknown file is no error.
 *
 * Each OPEN and IOCTL is answered with nvos_chardev_reply.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"

#ifndef EBADF
#define EBADF   9
#endif
#ifndef ENODEV
#define ENODEV  19
#endif
#ifndef EPERM
#define EPERM   1
#endif
#ifndef EFAULT
#define EFAULT  14
#endif
#ifndef EINVAL
#define EINVAL  22
#endif
#ifndef ENOTTY
#define ENOTTY  25
#endif

/* An open file, by the kernel's identity for it. */
struct chardev_file {
    NvU64 file;
    nv_linux_file_private_t *nvlfp;
    /* An open of the render node (os/glue/drm.c) in place of nvlfp. */
    void *drm;
    struct chardev_file *next;
};

static struct chardev_file *files;
static nvos_mutex_t files_lock;

/* The request rate and the bytes copied for the clients, said every five
 * seconds while requests come: what decides whether the path wants shared
 * memory (N3c measurement). */
static NvU64 count_ioctl, count_mmap, count_other, bytes_in, bytes_out;
static struct nvos_timer *count_timer;

#define COUNT_SPAN 5

static NvU32 pool_most_now(void);
static void which_say(void);
static void which_note(NvU32 escape, const NvU32 *words, NvU32 size);

static void count_say(void *unused)
{
    NvU64 ioctl, mmap, other, in, out;

    (void)unused;
    ioctl = __atomic_exchange_n(&count_ioctl, 0, __ATOMIC_RELAXED);
    mmap = __atomic_exchange_n(&count_mmap, 0, __ATOMIC_RELAXED);
    other = __atomic_exchange_n(&count_other, 0, __ATOMIC_RELAXED);
    in = __atomic_exchange_n(&bytes_in, 0, __ATOMIC_RELAXED);
    out = __atomic_exchange_n(&bytes_out, 0, __ATOMIC_RELAXED);
    if (ioctl + mmap + other != 0)
        nv_printf(NV_DBG_ERRORS, "nvrm: chardev: %llu req/s (ioctl %llu, mmap %llu), copy-in %llu KB/s, copy-out %llu KB/s, %u threads at most\n",
                  (ioctl + mmap + other) / COUNT_SPAN, ioctl / COUNT_SPAN, mmap / COUNT_SPAN,
                  in / COUNT_SPAN / 1024, out / COUNT_SPAN / 1024, pool_most_now());
    which_say();
    nvos_timer_start(count_timer, (NvU64)COUNT_SPAN * 1000000000ULL);
}

/* ------------------------------------------------------------------------
 * The stall diagnostic (2026-10-10, the desktop stall of nv-next on the
 * RTX 3060): which requests are in nvrm and not answered. On that boot two
 * ioctl threads never finished and nothing said which calls they were.
 *
 * Every ioctl is kept in a table from the moment it is taken until it is
 * answered: its id, the program's pid, the node, the command, and for an RM
 * call what os/kept/nv.c read of it (nvrm_diag_rm: the escape and its first
 * four words, which are a control's client, object and command and an
 * allocation's parent, handle and class). A thread of its own, which takes
 * none of RM's locks and no lock of the glue but the table's, says every
 * five seconds each request that has been in for five or more, and says so
 * too when the dispatch thread itself has been inside one request that
 * long: nothing is taken from the kernel meanwhile. A request said once is
 * said again when it is answered after all.
 *
 * One switch, nvrm_diag, which os/glue/drm.c reads too, and which turns
 * nvos's table of lock holders and sleepers on (nvos_lock_watch). Off: set
 * it to 1 and rebuild to look into a stall.
 * ---------------------------------------------------------------------- */
int nvrm_diag = 0;

#define DIAG_SLOTS      128
#define DIAG_OLD_NS     (5ULL * 1000000000ULL)
#define DIAG_LOOK_MS    1000
/* The most requests said in one look; the rest are counted. */
#define DIAG_SAID       16

struct diag_request {
    NvBool used;
    NvBool drm;
    NvBool rm_known;
    NvU64 id;
    NvU64 began;
    NvU64 said;         /* when it was last said, 0 if never */
    NvU64 thread;       /* the worker serving it, 0 before it runs */
    NvU32 pid;
    NvU32 minor;
    NvU32 cmd;
    NvU32 escape;
    NvU32 words[4];
};

static struct diag_request diag_table[DIAG_SLOTS];
static nvos_mutex_t diag_lock;
/* Requests that found the table full, so are not watched. */
static NvU64 diag_unwatched;

/* What the dispatch thread is inside, 0 in `began` when it waits for the
 * next request. Written by that thread alone, read by the watcher. */
static NvU64 diag_dispatch_began;
static NvU32 diag_dispatch_op, diag_dispatch_pid, diag_dispatch_minor;
static NvU64 diag_dispatch_id;

/*
 * A line of the diagnostic's own. Not through nv_printf: that takes the
 * print lock, and the watcher must be able to speak whatever another thread
 * holds. One write a line, so it is not cut by another thread's.
 */
__attribute__((format(printf, 1, 2))) static void diag_line(const char *format, ...)
{
    char line[320];
    va_list arguments;
    int used = snprintf(line, sizeof(line), "nvrm: diag: ");

    va_start(arguments, format);
    (void)vsnprintf(line + used, sizeof(line) - (size_t)used - 1, format, arguments);
    va_end(arguments);
    used = (int)strlen(line);
    line[used] = '\n';
    line[used + 1] = '\0';
    nvos_write_line(line);
}

static const char *diag_op_name(NvU32 op)
{
    switch (op)
    {
        case NVOS_REQUEST_OPEN:           return "OPEN";
        case NVOS_REQUEST_IOCTL:          return "IOCTL (starting its thread)";
        case NVOS_REQUEST_RELEASE:        return "RELEASE";
        case NVOS_REQUEST_MMAP:           return "MMAP";
        case NVOS_REQUEST_DMABUF_RELEASE: return "DMABUF_RELEASE";
        default:                          return "an unknown request";
    }
}

/* Keep an ioctl just taken; its slot, or -1. */
static int diag_enter(const struct nvos_request *request, NvBool drm)
{
    int slot = -1, i;

    if (!nvrm_diag)
        return -1;
    nvos_mutex_lock(&diag_lock);
    for (i = 0; i < DIAG_SLOTS; i++)
    {
        if (!diag_table[i].used)
        {
            os_mem_set(&diag_table[i], 0, sizeof(diag_table[i]));
            diag_table[i].used = NV_TRUE;
            diag_table[i].drm = drm;
            diag_table[i].id = request->id;
            diag_table[i].began = os_get_monotonic_time_ns();
            diag_table[i].pid = request->pid;
            diag_table[i].minor = request->minor;
            diag_table[i].cmd = request->cmd;
            slot = i;
            break;
        }
    }
    if (slot < 0)
        diag_unwatched++;
    nvos_mutex_unlock(&diag_lock);
    return slot;
}

/* The calling thread serves the request in `slot`. */
static void diag_serving(int slot)
{
    NvU64 thread = 0;

    if (slot < 0)
        return;
    (void)os_get_current_thread(&thread);
    nvos_mutex_lock(&diag_lock);
    diag_table[slot].thread = thread;
    nvos_mutex_unlock(&diag_lock);
}

/* The request in `slot` is answered, or was never started. */
static void diag_leave(int slot)
{
    struct diag_request was;

    if (slot < 0)
        return;
    nvos_mutex_lock(&diag_lock);
    was = diag_table[slot];
    diag_table[slot].used = NV_FALSE;
    nvos_mutex_unlock(&diag_lock);
    if (was.said != 0)
        diag_line("request %llu (pid %u, ioctl 0x%x) is answered after %llu ms",
                 (unsigned long long)was.id, was.pid, was.cmd,
                 (unsigned long long)((os_get_monotonic_time_ns() - was.began) / 1000000ULL));
}

/*
 * From os/kept/nv.c, once an RM ioctl's argument is read: which call the
 * calling thread's request is.
 */
void nvrm_diag_rm(NvU32 escape, const void *argument, NvU32 size)
{
    const NvU32 *words = argument;
    NvU64 thread = 0;
    int i, w;

    which_note(escape, words, size);
    if (!nvrm_diag)
        return;
    (void)os_get_current_thread(&thread);
    nvos_mutex_lock(&diag_lock);
    for (i = 0; i < DIAG_SLOTS; i++)
    {
        if (diag_table[i].used && diag_table[i].thread == thread && thread != 0)
        {
            diag_table[i].rm_known = NV_TRUE;
            diag_table[i].escape = escape;
            for (w = 0; w < 4; w++)
                diag_table[i].words[w] =
                    (words != NULL && size >= (NvU32)(w + 1) * sizeof(NvU32)) ? words[w] : 0;
            break;
        }
    }
    nvos_mutex_unlock(&diag_lock);
}

static void diag_say(const struct diag_request *request, NvU64 now)
{
    unsigned long long seconds = (unsigned long long)((now - request->began) / 1000000000ULL);
    char what[96];

    if (request->drm)
        snprintf(what, sizeof(what), "render ioctl 0x%x", request->cmd);
    else if (!request->rm_known)
        snprintf(what, sizeof(what), "%s ioctl 0x%x, argument not read yet",
                 request->minor == 255 ? "ctl" : "gpu", request->cmd);
    else if (request->escape == 0x2A)   /* NV_ESC_RM_CONTROL */
        snprintf(what, sizeof(what), "%s control 0x%x on object 0x%x of client 0x%x",
                 request->minor == 255 ? "ctl" : "gpu", request->words[2], request->words[1],
                 request->words[0]);
    else if (request->escape == 0x2B)   /* NV_ESC_RM_ALLOC */
        snprintf(what, sizeof(what), "%s alloc of class 0x%x as 0x%x under 0x%x of client 0x%x",
                 request->minor == 255 ? "ctl" : "gpu", request->words[3], request->words[2],
                 request->words[1], request->words[0]);
    else
        snprintf(what, sizeof(what), "%s escape 0x%x, words 0x%x 0x%x 0x%x 0x%x",
                 request->minor == 255 ? "ctl" : "gpu", request->escape, request->words[0],
                 request->words[1], request->words[2], request->words[3]);
    diag_line("request %llu of pid %u unanswered for %llu s on thread %llu: %s",
             (unsigned long long)request->id, request->pid, seconds,
             (unsigned long long)request->thread, what);
}

/* The watcher's thread. */
static void diag_watch(void *unused)
{
    static struct diag_request old[DIAG_SAID];
    NvU64 dispatch_said = 0, dispatch_said_for = 0;

    (void)unused;
    for (;;)
    {
        NvU64 now, began, unwatched;
        NvU32 count = 0, more = 0, in = 0;
        int i;

        (void)os_delay(DIAG_LOOK_MS);
        now = os_get_monotonic_time_ns();

        nvos_mutex_lock(&diag_lock);
        for (i = 0; i < DIAG_SLOTS; i++)
        {
            struct diag_request *request = &diag_table[i];

            if (!request->used)
                continue;
            in++;
            if (now - request->began < DIAG_OLD_NS ||
                (request->said != 0 && now - request->said < DIAG_OLD_NS))
                continue;
            if (count == DIAG_SAID)
            {
                more++;
                continue;
            }
            request->said = now;
            old[count++] = *request;
        }
        unwatched = diag_unwatched;
        nvos_mutex_unlock(&diag_lock);

        for (i = 0; i < (int)count; i++)
            diag_say(&old[i], now);
        if (count != 0)
            diag_line("%u request(s) are in nvrm%s; %llu were never watched (table full)",
                     in, more != 0 ? ", more old ones than are said at once" : "",
                     (unsigned long long)unwatched);
        /* What each sleeping thread sleeps for, and who holds it. */
        if (count != 0)
            nvos_lock_look();

        began = __atomic_load_n(&diag_dispatch_began, __ATOMIC_ACQUIRE);
        if (began != 0 && now - began >= DIAG_OLD_NS &&
            (dispatch_said_for != began || now - dispatch_said >= DIAG_OLD_NS))
        {
            dispatch_said = now;
            dispatch_said_for = began;
            diag_line("the dispatch thread has been inside %s (request %llu, pid %u, minor 0x%x) for %llu s; no request is taken meanwhile",
                     diag_op_name(diag_dispatch_op), (unsigned long long)diag_dispatch_id,
                     diag_dispatch_pid, diag_dispatch_minor,
                     (unsigned long long)((now - began) / 1000000000ULL));
        }
        else if (began == 0 && dispatch_said_for != 0)
        {
            diag_line("the dispatch thread takes requests again");
            dispatch_said_for = 0;
        }
        else if (began != 0 && dispatch_said_for != 0 && dispatch_said_for != began)
        {
            diag_line("the dispatch thread left the request it was inside");
            dispatch_said_for = 0;
        }
    }
}

static void remember(NvU64 file, nv_linux_file_private_t *nvlfp, void *drm,
                     struct chardev_file *entry)
{
    entry->file = file;
    entry->nvlfp = nvlfp;
    entry->drm = drm;
    nvos_mutex_lock(&files_lock);
    entry->next = files;
    files = entry;
    nvos_mutex_unlock(&files_lock);
}

static nv_linux_file_private_t *find(NvU64 file)
{
    struct chardev_file *entry;
    nv_linux_file_private_t *found = NULL;

    nvos_mutex_lock(&files_lock);
    for (entry = files; entry != NULL; entry = entry->next)
        if (entry->file == file)
        {
            found = entry->nvlfp;
            break;
        }
    nvos_mutex_unlock(&files_lock);
    return found;
}

/* The render node's open `file` is, or NULL for another file. */
static void *find_drm(NvU64 file)
{
    struct chardev_file *entry;
    void *found = NULL;

    nvos_mutex_lock(&files_lock);
    for (entry = files; entry != NULL; entry = entry->next)
        if (entry->file == file)
        {
            found = entry->drm;
            break;
        }
    nvos_mutex_unlock(&files_lock);
    return found;
}

/*
 * A RELEASE can be served before the OPEN of its file: the kernel queues it
 * the moment a program gives up on an OPEN, and two threads of the pool
 * take the two. Such a file is remembered here until its OPEN is through,
 * which then closes it at once. A RELEASE for an OPEN that was refused
 * stays in the ring until 32 more push it out: identities are never
 * reused, so it matches nothing.
 */
#define EARLY_SLOTS 32
static NvU64 early[EARLY_SLOTS];
static NvU32 early_next;

/* Under serial_lock. */
static void early_note(NvU64 file)
{
    early[early_next++ % EARLY_SLOTS] = file;
}

/* Whether `file` was released before now; forgotten if so. Under
 * serial_lock. */
static NvBool early_take(NvU64 file)
{
    NvU32 i;

    for (i = 0; i < EARLY_SLOTS; i++)
        if (early[i] == file && file != 0)
        {
            early[i] = 0;
            return NV_TRUE;
        }
    return NV_FALSE;
}

static struct chardev_file *forget(NvU64 file)
{
    struct chardev_file **at, *entry = NULL;

    nvos_mutex_lock(&files_lock);
    for (at = &files; *at != NULL; at = &(*at)->next)
        if ((*at)->file == file)
        {
            entry = *at;
            *at = entry->next;
            break;
        }
    nvos_mutex_unlock(&files_lock);
    return entry;
}

/* The client's copies, through the bridge, for the request in context. */
struct worker;
static int bridge_in(void *context, void *to, NvU64 from, NvU32 length);
static int bridge_out(void *context, NvU64 to, const void *from, NvU32 length);
static NvS64 bridge_fd(void *context, int fd);

/* ------------------------------------------------------------------------
 * The round trip's own figures (docs/NVIDIA.md §4.7): where one request's
 * time goes inside nvrm, for the program that made it to read back.
 *
 * Every ioctl is stamped with the processor's time-stamp counter when the
 * bridge hands it over (taken), when the thread that serves it begins
 * (serving) and when its handler has returned, before the answer is sent
 * (done), with the copies it made of the program's memory counted. The
 * stamps of a program's last ioctl are kept by its pid, and the ioctl
 * NVRM_TRIP_STAMPS on any of nvrm's files reads them: with its own stamps
 * around the call, a program has the request's four stages. The counter is
 * the machine's, so the program's stamps and nvrm's are on one clock.
 *
 * NVRM_TRIP_MODE is the bench's negative control, taken only where
 * nvrm_trip_control is set, which nvrm's main does on the test device
 * alone: mode 1 serves every ioctl on a thread started for it, as nvrm did
 * until 2026-10-10, so the gate can show its bound catches that.
 * ---------------------------------------------------------------------- */
#define NVRM_TRIP_TYPE      0xF7
#define TRIP_TYPE_OF(cmd)   (((cmd) >> _IOC_TYPESHIFT) & ((1U << _IOC_TYPEBITS) - 1))
#define NVRM_TRIP_STAMPS    _IOC(2U, NVRM_TRIP_TYPE, 1, sizeof(struct nvrm_trip_stamps))
#define NVRM_TRIP_MODE      _IOC(1U, NVRM_TRIP_TYPE, 2, sizeof(NvU32))

struct nvrm_trip_stamps {
    NvU64 id;       /* the request, 0 if the program made none before */
    NvU64 taken;    /* the bridge handed it to nvrm */
    NvU64 serving;  /* the thread that serves it began */
    NvU64 done;     /* its handler returned */
    NvU32 cmd;
    NvU32 copies;   /* copies of the program's memory, in and out */
    NvU32 pid;
    NvU32 mode;     /* how nvrm serves: NVRM_TRIP_MODE's */
};

int nvrm_trip_control = 0;
/* 0 from the pool, 1 a thread started for every ioctl. */
static NvU32 trip_mode;

#define TRIP_SLOTS 16
static struct { NvU32 busy; struct nvrm_trip_stamps last; } trip_table[TRIP_SLOTS];

static inline NvU64 trip_now(void)
{
    return __builtin_ia32_rdtsc();
}

/* Keep `stamps` as its program's last; dropped if another thread is at the
 * slot, which only loses a figure. */
static void trip_keep(const struct nvrm_trip_stamps *stamps)
{
    NvU32 slot = stamps->pid % TRIP_SLOTS;

    if (__atomic_exchange_n(&trip_table[slot].busy, 1, __ATOMIC_ACQUIRE) != 0)
        return;
    trip_table[slot].last = *stamps;
    __atomic_store_n(&trip_table[slot].busy, 0, __ATOMIC_RELEASE);
}

/* One ioctl's worker. */
struct worker {
    struct nvos_request request;
    nv_linux_file_private_t *nvlfp;
    void *drm;
    /* Its place in the diagnostic's table, or -1. */
    int diag;
    /* The round trip's stamps, and the copies counted. */
    struct nvrm_trip_stamps trip;
    /* For an RM call, once its argument is read: the escape, and a
     * control's command or an allocation's class (which_note). */
    NvU32 escape, detail;
    /* Whether the dispatcher made it: nvrm-link-test's client has a
     * context of its own. */
    NvU32 magic;
    /* The first bytes of the ioctl's argument, as they came with the
     * request: head_length of them, none once the argument is written. */
    NvU32 head_length;
    /* What the handler wrote at the ioctl's argument, kept to go back with
     * the answer: tail_length bytes, where keep_tail lets it be kept. */
    NvU32 tail_length;
    NvBool keep_tail;
    NvU8 head[NVOS_INLINE_MAX];
    NvU8 tail[NVOS_INLINE_MAX];
};

#define WORKER_MAGIC 0x74726970u

/* ------------------------------------------------------------------------
 * Which requests come (docs/NVIDIA.md §4.7): every ioctl served is counted
 * by its command and, for an RM call, by its escape and its control's
 * command or its allocation's class, in a small table without a lock; the
 * line said every five seconds names the four most frequent. A table that
 * is full counts the rest together.
 * ---------------------------------------------------------------------- */
#define WHICH_SLOTS 64

static struct { NvU64 key; NvU64 count; } which_table[WHICH_SLOTS];
static NvU64 which_lost;

static void which_count(const struct worker *work)
{
    /* The command, then the escape's detail; never 0 for a real ioctl. */
    NvU64 key = ((NvU64)work->request.cmd << 32) | work->detail;
    NvU32 at = (NvU32)((key * 0x9E3779B97F4A7C15ULL) >> 58), step;

    for (step = 0; step < 8; step++)
    {
        NvU64 was = __atomic_load_n(&which_table[(at + step) % WHICH_SLOTS].key, __ATOMIC_RELAXED);

        if (was == 0 &&
            __atomic_compare_exchange_n(&which_table[(at + step) % WHICH_SLOTS].key, &was, key,
                                        NV_FALSE, __ATOMIC_RELAXED, __ATOMIC_RELAXED))
            was = key;
        if (was == key)
        {
            __atomic_fetch_add(&which_table[(at + step) % WHICH_SLOTS].count, 1, __ATOMIC_RELAXED);
            return;
        }
    }
    __atomic_fetch_add(&which_lost, 1, __ATOMIC_RELAXED);
}

/* From nvrm_diag_rm, once an RM call's argument is read: which call the
 * calling thread's request is -- a control's command, an allocation's
 * class, or the escape alone. */
static void which_note(NvU32 escape, const NvU32 *words, NvU32 size)
{
    struct worker *work = nvos_client_context();

    if (work == NULL || work->magic != WORKER_MAGIC)
        return;
    work->escape = escape;
    if (escape == 0x2A && words != NULL && size >= 3 * sizeof(NvU32))        /* NV_ESC_RM_CONTROL */
        work->detail = words[2];
    else if (escape == 0x2B && words != NULL && size >= 4 * sizeof(NvU32))   /* NV_ESC_RM_ALLOC */
        work->detail = words[3];
}

/* Say the four most frequent of the span, and empty the table. */
static void which_say(void)
{
    struct { NvU64 key, count; } top[4] = { { 0, 0 } };
    char line[256];
    int used, i, j;
    NvU64 lost = __atomic_exchange_n(&which_lost, 0, __ATOMIC_RELAXED);

    for (i = 0; i < WHICH_SLOTS; i++)
    {
        NvU64 key = __atomic_exchange_n(&which_table[i].key, 0, __ATOMIC_RELAXED);
        NvU64 count = __atomic_exchange_n(&which_table[i].count, 0, __ATOMIC_RELAXED);

        for (j = 0; j < 4 && key != 0; j++)
            if (count > top[j].count)
            {
                NvU64 k = top[j].key, c = top[j].count;

                top[j].key = key;
                top[j].count = count;
                key = k;
                count = c;
            }
    }
    if (top[0].count == 0)
        return;
    used = snprintf(line, sizeof(line), "nvrm: chardev: most frequent:");
    for (j = 0; j < 4 && top[j].count != 0; j++)
        used += snprintf(line + used, sizeof(line) - (size_t)used, " ioctl 0x%x/0x%x %llu/s;",
                         (NvU32)(top[j].key >> 32), (NvU32)top[j].key,
                         (unsigned long long)(top[j].count / COUNT_SPAN));
    nv_printf(NV_DBG_ERRORS, "%s %llu uncounted\n", line, (unsigned long long)lost);
}


/* The trip ioctls, answered here: 0 or a negative errno. */
static int trip_ioctl(struct worker *work)
{
    NvU32 slot = work->request.pid % TRIP_SLOTS;
    struct nvrm_trip_stamps last;
    NvU32 mode;

    switch (work->request.cmd)
    {
        case NVRM_TRIP_STAMPS:
            while (__atomic_exchange_n(&trip_table[slot].busy, 1, __ATOMIC_ACQUIRE) != 0)
                ;
            last = trip_table[slot].last;
            __atomic_store_n(&trip_table[slot].busy, 0, __ATOMIC_RELEASE);
            if (last.pid != work->request.pid)
                os_mem_set(&last, 0, sizeof(last));
            last.mode = __atomic_load_n(&trip_mode, __ATOMIC_RELAXED);
            return nvos_chardev_copy_out(work->request.id, work->request.arg, &last,
                                         sizeof(last)) == 0 ? 0 : -EFAULT;
        case NVRM_TRIP_MODE:
            if (!nvrm_trip_control)
                return -EPERM;
            if (nvos_chardev_copy_in(work->request.id, &mode, work->request.arg,
                                     sizeof(mode)) != 0)
                return -EFAULT;
            if (mode > 1)
                return -EINVAL;
            __atomic_store_n(&trip_mode, mode, __ATOMIC_RELAXED);
            return 0;
        default:
            return -ENOTTY;
    }
}

/*
 * The client's copies, through the bridge, for the request its worker
 * serves (docs/NVIDIA.md §4.7).
 *
 * Two of them need no call: a read inside the first bytes of the ioctl's
 * argument is answered from the bytes that came with the request, which
 * the program's own thread read as it made the call; and one write at the
 * argument's start is kept, to go back with the answer, where the program's
 * own thread writes it before its call returns. Kept bytes are written out
 * at once, by a copy of the old kind, before any later copy that touches
 * the same bytes, and before an answer that is an error, with which the
 * kernel sends no bytes back; and once the argument has been written its
 * first bytes are no longer read from what came with the request.
 */
static NvBool tail_touches(const struct worker *work, NvU64 at, NvU32 length)
{
    return work->tail_length != 0 && at < work->request.arg + work->tail_length &&
           work->request.arg < at + length;
}

/* Write the kept bytes out now; 0, or -EFAULT. */
static int tail_flush(struct worker *work)
{
    NvU32 length = work->tail_length;

    if (length == 0)
        return 0;
    work->tail_length = 0;
    work->trip.copies++;
    return nvos_chardev_copy_out(work->request.id, work->request.arg, work->tail, length) == 0
               ? 0 : -EFAULT;
}

static int bridge_in(void *context, void *to, NvU64 from, NvU32 length)
{
    struct worker *work = context;

    __atomic_fetch_add(&bytes_in, length, __ATOMIC_RELAXED);
    if (tail_touches(work, from, length) && tail_flush(work) != 0)
        return -EFAULT;
    if (from >= work->request.arg && length <= work->head_length &&
        from - work->request.arg <= work->head_length - length)
    {
        memcpy(to, work->head + (from - work->request.arg), length);
        return 0;
    }
    work->trip.copies++;
    return nvos_chardev_copy_in(work->request.id, to, from, length) == 0 ? 0 : -EFAULT;
}

static int bridge_out(void *context, NvU64 to, const void *from, NvU32 length)
{
    struct worker *work = context;

    __atomic_fetch_add(&bytes_out, length, __ATOMIC_RELAXED);
    if (tail_touches(work, to, length) && tail_flush(work) != 0)
        return -EFAULT;
    /* Written, the argument is no longer what came with the request. */
    if (to < work->request.arg + work->head_length && work->request.arg < to + length)
        work->head_length = 0;
    if (work->keep_tail && to == work->request.arg && length != 0 && length <= NVOS_INLINE_MAX)
    {
        memcpy(work->tail, from, length);
        work->tail_length = length;
        return 0;
    }
    work->trip.copies++;
    return nvos_chardev_copy_out(work->request.id, to, from, length) == 0 ? 0 : -EFAULT;
}

static NvS64 bridge_fd(void *context, int fd)
{
    struct worker *work = context;

    return nvos_chardev_file(work->request.id, fd);
}

/* Serve the ioctl in `work` as its client: its answer, a value or a
 * negative errno, for the caller to send. */
static NvS64 serve(struct worker *work)
{
    nvos_client_t client;
    int rc;

    work->trip.serving = trip_now();
    diag_serving(work->diag);

    os_mem_set(&client, 0, sizeof(client));
    client.pid = work->request.pid;
    client.euid = work->request.euid;
    client.administrator = (work->request.euid == 0);
    client.copy_in = bridge_in;
    client.copy_out = bridge_out;
    client.context = work;
    client.resolve_fd = bridge_fd;

    if (!nvos_client_enter(&client))
    {
        diag_leave(work->diag);
        return -EBUSY;
    }
    if (TRIP_TYPE_OF(work->request.cmd) == NVRM_TRIP_TYPE)
        rc = trip_ioctl(work);
    else if (work->drm != NULL)
        rc = nvrm_drm_ioctl(work->drm, work->request.id, work->request.cmd, work->request.arg);
    else
        rc = nvrm_ioctl(work->nvlfp, work->request.cmd, (void *)(NvUPtr)work->request.arg);
    nvos_client_leave();
    /* No bytes go back with an error: what was kept is written now. */
    if (rc < 0)
        (void)tail_flush(work);
    if (TRIP_TYPE_OF(work->request.cmd) != NVRM_TRIP_TYPE)
    {
        work->trip.id = work->request.id;
        work->trip.cmd = work->request.cmd;
        work->trip.pid = work->request.pid;
        work->trip.done = trip_now();
        trip_keep(&work->trip);
        which_count(work);
    }
    diag_leave(work->diag);
    return rc;
}

/* A thread started for one ioctl (NVRM_TRIP_MODE 1). */
static void serve_ioctl(void *argument)
{
    struct worker *work = argument;
    NvS64 rc = serve(work);

    (void)nvos_chardev_reply(work->request.id, rc < 0 ? (int)rc : 0, rc > 0 ? rc : 0);
    free(argument);
}

#ifndef ENXIO
#define ENXIO   6
#endif
#ifndef ERANGE
#define ERANGE  34
#endif

/* An MMAP: what the file's mapping context names, as nvidia_mmap_helper
 * reads it. */
static void serve_mmap(const struct nvos_request *request)
{
    nv_linux_file_private_t *nvlfp = find(request->file);
    const nv_alloc_mapping_context_t *context;
    nv_state_t *nv;
    NvU64 length = (NvU64)request->pages << PAGE_SHIFT;

    if (nvlfp == NULL)
    {
        (void)nvos_chardev_reply(request->id, -EBADF, 0);
        return;
    }
    context = &nvlfp->mmap_context;
    nv = NV_STATE_PTR(nvlfp->nvptr);
    if (!context->valid || request->arg != 0)
    {
        nvrm_say("mmap: no valid mapping context on the file\n");
        (void)nvos_chardev_reply(request->id, -EINVAL, 0);
        return;
    }
    if (!NV_IS_CTL_DEVICE(nv))
    {
        NvU64 kind = NVOS_MAP_APERTURE;

        if (context->memArea.numRanges != 1 ||
            context->memArea.pRanges[0].size != length)
        {
            nvrm_say("mmap: %llu ranges, or not %llu bytes\n",
                     (unsigned long long)context->memArea.numRanges,
                     (unsigned long long)length);
            (void)nvos_chardev_reply(request->id, -ENXIO, 0);
            return;
        }
        if (IS_FB_OFFSET(nv, context->access_start, context->access_size) &&
            !IS_UD_OFFSET(nv, context->access_start, context->access_size) &&
            context->caching == NV_MEMORY_WRITECOMBINED)
            kind |= NVOS_MAP_WRITE_COMBINING;
        (void)nvos_chardev_reply_map(request->id, context->memArea.pRanges[0].start, 0, kind);
        return;
    }
    else
    {
        nv_alloc_t *at = context->alloc;
        NvU32 vmo = 0;

        if (at == NULL || context->page_index + request->pages > at->num_pages)
        {
            (void)nvos_chardev_reply(request->id, -ERANGE, 0);
            return;
        }
        if (nvos_pages_vmo(at->pages, &vmo) != NV_OK)
        {
            (void)nvos_chardev_reply(request->id, -ENXIO, 0);
            return;
        }
        (void)nvos_chardev_reply_map(request->id, vmo, context->page_index << PAGE_SHIFT,
                                     NVOS_MAP_VMO);
    }
}

/* Make `work` the ioctl `request` is, on the file it names; answered
 * EBADF, and false, for a file nvrm does not have. */
static NvBool take(const struct nvos_request *request, NvU64 taken, struct worker *work)
{
    nv_linux_file_private_t *nvlfp = find(request->file);
    void *drm = find_drm(request->file);

    if (nvlfp == NULL && drm == NULL)
    {
        (void)nvos_chardev_reply(request->id, -EBADF, 0);
        return NV_FALSE;
    }
    os_mem_set(&work->trip, 0, sizeof(work->trip));
    work->tail_length = 0;
    work->escape = 0;
    work->detail = 0;
    work->magic = WORKER_MAGIC;
    work->request = *request;
    work->trip.taken = taken;
    work->nvlfp = nvlfp;
    work->drm = drm;
    work->diag = diag_enter(request, drm != NULL);
    return NV_TRUE;
}

static void handle_one(const struct nvos_request *request, NvU64 taken);

static void count(const struct nvos_request *request)
{
    if (request->op == NVOS_REQUEST_IOCTL)
        __atomic_fetch_add(&count_ioctl, 1, __ATOMIC_RELAXED);
    else if (request->op == NVOS_REQUEST_MMAP)
        __atomic_fetch_add(&count_mmap, 1, __ATOMIC_RELAXED);
    else
        __atomic_fetch_add(&count_other, 1, __ATOMIC_RELAXED);
}

/* One request served where it was taken, with the watcher told what the
 * thread that took it is inside: every request but a pool's ioctl. */
static void handle(const struct nvos_request *request, NvU64 taken)
{
    count(request);
    if (nvrm_diag)
    {
        diag_dispatch_op = request->op;
        diag_dispatch_pid = request->pid;
        diag_dispatch_minor = request->minor;
        diag_dispatch_id = request->id;
        __atomic_store_n(&diag_dispatch_began, os_get_monotonic_time_ns(), __ATOMIC_RELEASE);
    }
    handle_one(request, taken);
    __atomic_store_n(&diag_dispatch_began, 0, __ATOMIC_RELEASE);
}

static void handle_one(const struct nvos_request *request, NvU64 taken)
{
    nv_linux_file_private_t *nvlfp;
    struct chardev_file *entry;
    struct worker *work;
    void *drm;

    switch (request->op)
    {
        case NVOS_REQUEST_OPEN:
            entry = calloc(1, sizeof(*entry));
            if (entry == NULL)
            {
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
                return;
            }
            if (request->minor == NVRM_RENDER_MINOR)
            {
                drm = nvrm_drm_open();
                if (drm == NULL)
                {
                    free(entry);
                    (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
                    return;
                }
                if (early_take(request->file))
                {
                    nvrm_drm_close(drm);
                    free(entry);
                    (void)nvos_chardev_reply(request->id, -ENODEV, 0);
                    return;
                }
                remember(request->file, NULL, drm, entry);
                (void)nvos_chardev_reply(request->id, 0, 0);
                return;
            }
            nvlfp = (request->minor == 255) ? nvrm_open_ctl() : nvrm_open_gpu(request->minor);
            if (nvlfp == NULL)
            {
                free(entry);
                (void)nvos_chardev_reply(request->id, -ENODEV, 0);
                return;
            }
            /* RM names files by fd (nv_get_file_private): the kernel's
             * identity for it, which chardev_file resolves descriptors to. */
            nvlfp->fd = (NvS32)request->file;
            if (early_take(request->file))
            {
                nvrm_close(nvlfp);
                free(entry);
                (void)nvos_chardev_reply(request->id, -ENODEV, 0);
                return;
            }
            remember(request->file, nvlfp, NULL, entry);
            (void)nvos_chardev_reply(request->id, 0, 0);
            return;

        case NVOS_REQUEST_IOCTL:
            /* Only where a thread is started for every ioctl: the pool's
             * threads serve an ioctl themselves (pool_run). */
            work = calloc(1, sizeof(*work));
            if (work == NULL)
            {
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
                return;
            }
            if (!take(request, taken, work))
            {
                free(work);
                return;
            }
            if (!nvos_thread_spawn(serve_ioctl, work))
            {
                diag_leave(work->diag);
                free(work);
                (void)nvos_chardev_reply(request->id, -ENOMEM, 0);
            }
            return;

        case NVOS_REQUEST_MMAP:
            drm = find_drm(request->file);
            if (drm != NULL)
                nvrm_drm_mmap(drm, request);
            else
                serve_mmap(request);
            return;

        case NVOS_REQUEST_RELEASE:
            entry = forget(request->file);
            if (entry != NULL)
            {
                if (entry->drm != NULL)
                    nvrm_drm_close(entry->drm);
                else
                    nvrm_close(entry->nvlfp);
                free(entry);
            }
            else
                early_note(request->file);
            return;

        case NVOS_REQUEST_DMABUF_RELEASE:
            nvrm_drm_released(request->arg);
            return;

        default:
            return;
    }
}

int nvrm_chardev_publish(const NvU16 *minors, NvU32 count)
{
    return nvos_chardev_start(minors, count) == NV_OK ? 0 : -1;
}

/* ------------------------------------------------------------------------
 * The pool (docs/NVIDIA.md §4.7). Until 2026-10-10 one thread took every
 * request and started a thread for each ioctl: a stack mapped, a clone, an
 * exit and an unmap a request, 116 us of a 544 us round trip on the bench.
 *
 * Now the threads that serve are the threads that take. Each waits in the
 * kernel for a request (nvos_chardev_next); the kernel hands a request to
 * the one that began to wait last and wakes it where the program that made
 * the request ran. The thread serves it on its own stack, with nothing
 * allocated, and answers it in the call that takes its next. A thread that
 * takes a request and leaves none waiting starts one, up to POOL_MOST,
 * which is the number of clients nvos lets threads serve at once; at that
 * number the last free thread answers ioctls EBUSY, as nvos_client_enter's
 * refusal did, so that requests are still taken.
 *
 * Everything but an ioctl -- OPEN, RELEASE, MMAP, a dmabuf's release -- is
 * still served one at a time, under serial_lock, as the one dispatch
 * thread served it. Their order is no longer the kernel's, since two
 * threads take two requests; the one order that mattered, a RELEASE behind
 * the OPEN of its file, is kept by the file (early_note).
 * ---------------------------------------------------------------------- */
#define POOL_MOST 64

static nvos_mutex_t serial_lock, pool_lock;
/* The pool's threads, and how many of them serve nothing. Under pool_lock. */
static NvU32 pool_threads, pool_free;
/* The most threads the pool has had, for the line said at 5 s. */
static NvU32 pool_most;

static void pool_run(void *unused);

/* The calling thread has a request to serve: whether another thread is, or
 * now will be, free to take the next. */
static NvBool pool_engage(void)
{
    NvBool other = NV_TRUE, start = NV_FALSE;

    nvos_mutex_lock(&pool_lock);
    pool_free--;
    if (pool_free == 0)
    {
        if (pool_threads < POOL_MOST)
        {
            pool_threads++;
            pool_free++;
            if (pool_threads > pool_most)
                pool_most = pool_threads;
            start = NV_TRUE;
        }
        else
        {
            pool_free++;
            other = NV_FALSE;
        }
    }
    nvos_mutex_unlock(&pool_lock);
    if (start && !nvos_thread_spawn_plain(pool_run, NULL))
    {
        nvos_mutex_lock(&pool_lock);
        pool_threads--;
        /* This thread stays free instead of the one that did not start. */
        nvos_mutex_unlock(&pool_lock);
        other = NV_FALSE;
    }
    return other;
}

static void pool_release(void)
{
    nvos_mutex_lock(&pool_lock);
    pool_free++;
    nvos_mutex_unlock(&pool_lock);
}

static void pool_run(void *unused)
{
    struct worker *work = calloc(1, sizeof(*work));
    struct nvos_request request;
    /* The answer that goes with the next take: 0 for none. */
    NvU64 answer = 0;
    NvS64 result = 0;
    NvU64 taken;

    (void)unused;
    /* Without memory for its own, a thread takes and refuses. */
    for (;;)
    {
        NvU32 head_length = 0;
        NvU8 spare[NVOS_INLINE_MAX];

        if (nvos_chardev_next(&request, work != NULL ? work->head : spare, &head_length,
                              answer, result, work != NULL ? work->tail : spare,
                              work != NULL ? work->tail_length : 0) != NV_OK)
            return;
        taken = trip_now();
        answer = 0;

        if (request.op == NVOS_REQUEST_IOCTL &&
            __atomic_load_n(&trip_mode, __ATOMIC_RELAXED) != 0)
        {
            /* The control: a thread started for it, as the one dispatch
             * thread did. */
            handle(&request, taken);
            continue;
        }
        if (request.op != NVOS_REQUEST_IOCTL)
        {
            /* One at a time. A thread that is the last one free serves it
             * all the same: nothing here waits for a client. */
            NvBool other = pool_engage();

            nvos_mutex_lock(&serial_lock);
            handle(&request, taken);
            nvos_mutex_unlock(&serial_lock);
            if (other)
                pool_release();
            continue;
        }

        count(&request);
        if (work == NULL || !pool_engage())
        {
            (void)nvos_chardev_reply(request.id, work == NULL ? -ENOMEM : -EBUSY, 0);
            continue;
        }
        if (take(&request, taken, work))
        {
            work->head_length = head_length;
            work->keep_tail = NV_TRUE;
            result = serve(work);
            answer = request.id;
        }
        /* Free before the answer goes: the program's next request can come
         * before this thread is back in the kernel's wait, and whoever
         * took it would start a thread for want of a free one. */
        pool_release();
    }
}

int nvrm_chardev_serve(void)
{
    count_timer = nvos_timer_create(count_say, NULL);
    if (count_timer != NULL)
        nvos_timer_start(count_timer, (NvU64)COUNT_SPAN * 1000000000ULL);
    if (nvrm_diag)
    {
        nvos_lock_watch(NV_TRUE);
        if (nvos_thread_spawn(diag_watch, NULL))
            diag_line("on: requests unanswered for 5 s are said, every 5 s");
        else
            diag_line("no thread for the watcher; nothing will be said");
    }
    pool_threads = pool_free = pool_most = 1;
    pool_run(NULL);
    return -2;
}

static NvU32 pool_most_now(void)
{
    return __atomic_load_n(&pool_most, __ATOMIC_RELAXED);
}
