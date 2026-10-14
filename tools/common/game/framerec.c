/* framerec: the frame recorder, an LD_PRELOAD library for stage 24's rig
 * (docs/GAME-PATH.md §2.4, R1). The same binary runs on Ferrix and on the
 * Linux baseline guest, under any x86-64 glibc program that draws through
 * GLX or EGL.
 *
 * What it does, per swap (glXSwapBuffers or eglSwapBuffers):
 *
 *  - records CLOCK_MONOTONIC at the swap's entry and at its return;
 *  - for a GLX window, asks the X server through the Present extension for
 *    the window's current media stream counter (msc) and its time (ust),
 *    with a PresentNotifyMSC of target 0 sent right after the swap returns,
 *    on a second X connection of its own. The server answers with a
 *    CompleteNotify whatever way the program draws (a DRI3 PresentPixmap, or
 *    llvmpipe's XPutImage), so every swap gets an msc. The same connection
 *    selects CompleteNotify on the window, so when the program presents by
 *    PresentPixmap, its own completions (the msc each frame was shown at)
 *    are recorded too.
 *
 * A missed refresh is an msc that advanced by more than one between two
 * swaps of a vsynced program: the same definition on both systems, with no
 * tracing in either kernel. `cargo xtask frames <record>` reads the record
 * and gives the median, 99th and 99.9th percentile frame times, the misses
 * a minute and the worst frame.
 *
 * The record goes to the file $FRAMEREC_OUT names (a `%p` in it becomes the
 * process id), as text, one line per event:
 *
 *     framerec 1 pid <pid>             the header
 *     s <swap> <entry ns> <return ns>  a swap, CLOCK_MONOTONIC
 *     x <swap> <ms>                    a stall of <ms> injected before it
 *     c <serial> <n|p> <ust us> <msc>  a completion: n = the recorder's
 *                                      NotifyMSC for swap <serial>, p = the
 *                                      program's own PresentPixmap
 *     e <what> <value>                 something that went wrong
 *
 * It is kept in a ring in memory and written out when the ring is full, at
 * exit, and on SIGTERM; so a program that ends with _exit or is killed with
 * SIGKILL loses what was not written yet.
 *
 * The negative control: FRAMEREC_STALL_MS=<ms> sleeps that long inside the
 * swap, before the real one, once every FRAMEREC_STALL_EVERY seconds
 * (default 10) from the first swap, as a frame that took that much longer
 * to make. At 60 Hz a 50 ms stall is three missed refreshes.
 *
 * It also prints, as the baseline's swapfps did, one line a second while
 * the program swaps, to stderr or appended to the file $SWAPFPS_LOG names:
 *
 *     swap-fps N glx G egl E rawmotion R motion M ms T
 *
 * N = swaps in the interval scaled to one second, G/E = raw counts by entry
 * point, R/M = XInput2 raw-motion / motion events the program fetched with
 * XGetEventData, T = the interval's real length in milliseconds.
 *
 * SDL2 dlopen()s libGL/libEGL/libX11 and looks its entry points up with
 * dlsym(handle, ...), which LD_PRELOAD does not interpose, and glXSwapBuffers
 * with glXGetProcAddress(ARB); so dlsym and glXGetProcAddress(ARB) are
 * wrapped here and hand out the recording functions for the names below.
 * Programs that link the functions directly reach the exported ones.
 * libxcb and libxcb-present are opened at run time, so the library has no
 * link-time dependency but the C library, and records times without them.
 *
 * Only GLIBC_2.2.5 symbols are used, so it loads on any x86_64 glibc:
 *   cc -O2 -fPIC -shared -fno-stack-protector -U_FORTIFY_SOURCE \
 *       -o libframerec.so framerec.c -ldl
 * (`cargo xtask frames --build <dir>` builds it so.)
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

__asm__(".symver dlvsym,dlvsym@GLIBC_2.2.5");
__asm__(".symver dlopen,dlopen@GLIBC_2.2.5");

typedef void *(*dlsym_fn)(void *, const char *);
static dlsym_fn real_dlsym;

static void (*real_glx_swap)(void *, unsigned long);
static unsigned (*real_egl_swap)(void *, void *);
static int (*real_get_event_data)(void *, void *);
static unsigned long (*real_glx_create_window)(void *, void *, unsigned long, const int *);

static volatile long n_glx, n_egl, n_raw, n_motion;
static long long last_ms;
static int out_fd = -1;
static volatile int printing;

static void find_dlsym(void)
{
    if (real_dlsym)
        return;
    real_dlsym = (dlsym_fn)dlvsym(RTLD_NEXT, "dlsym", "GLIBC_2.2.5");
    if (!real_dlsym)
        real_dlsym = (dlsym_fn)dlvsym(RTLD_NEXT, "dlsym", "GLIBC_2.34");
    if (!real_dlsym) {
        static const char m[] = "framerec: no dlsym in the C library\n";
        if (write(2, m, sizeof m - 1)) { }
        _exit(127);
    }
}

/* CLOCK_MONOTONIC in nanoseconds, by the raw system call: no vDSO and no
 * librt needed, and the same clock an X server's ust is on. */
static uint64_t now_ns(void)
{
    struct timespec ts = {0, 0};
    syscall(SYS_clock_gettime, 1 /* CLOCK_MONOTONIC */, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static long long now_ms(void)
{
    /* +1 so that 0 means "not started". */
    return (long long)(now_ns() / 1000000u) + 1;
}

static void say(const char *line, int len)
{
    if (out_fd < 0) {
        const char *path = getenv("SWAPFPS_LOG");
        out_fd = path ? open(path, O_WRONLY | O_CREAT | O_APPEND, 0644) : 2;
        if (out_fd < 0)
            out_fd = 2;
    }
    if (len > 0 && write(out_fd, line, (size_t)len)) { }
}

static void tick(void)
{
    long long now = now_ms();
    if (!last_ms) {
        static const char first[] = "swapfps: first swap\n";
        last_ms = now;
        say(first, sizeof first - 1);
        return;
    }
    long long dt = now - last_ms;
    if (dt < 1000 || __sync_lock_test_and_set(&printing, 1))
        return;
    last_ms = now;
    long g = __sync_lock_test_and_set(&n_glx, 0);
    long e = __sync_lock_test_and_set(&n_egl, 0);
    long r = __sync_lock_test_and_set(&n_raw, 0);
    long m = __sync_lock_test_and_set(&n_motion, 0);
    char line[160];
    int len = snprintf(line, sizeof line,
                       "swap-fps %lld glx %ld egl %ld rawmotion %ld motion %ld ms %lld\n",
                       ((g + e) * 1000 + dt / 2) / dt, g, e, r, m, dt);
    say(line, len);
    __sync_lock_release(&printing);
}

/* ---- The record ---------------------------------------------------- */

enum { REC_SWAP = 's', REC_STALL = 'x', REC_NOTIFY = 'n', REC_PIXMAP = 'p', REC_ERROR = 'e' };

struct rec {
    uint32_t what;
    uint32_t serial;
    uint64_t a;
    uint64_t b;
};

#define RING 16384
static struct rec ring[RING];
static unsigned ring_used;
static int rec_fd = -2; /* -2: not opened yet; -1: not recording */
static volatile int rec_lock;
static int header_written;

static void rec_open(void)
{
    if (rec_fd != -2)
        return;
    rec_fd = -1;
    const char *path = getenv("FRAMEREC_OUT");
    if (!path || !*path)
        return;
    char name[512];
    size_t at = 0;
    for (const char *p = path; *p && at + 12 < sizeof name; p++) {
        if (p[0] == '%' && p[1] == 'p') {
            at += (size_t)snprintf(name + at, sizeof name - at, "%d", (int)getpid());
            p++;
        } else {
            name[at++] = *p;
        }
    }
    name[at] = 0;
    rec_fd = open(name, O_WRONLY | O_CREAT | O_TRUNC, 0644);
}

/* A decimal number, without the C library's formatting: the writer runs in
 * a signal handler too. */
static char *put_u64(char *at, uint64_t v)
{
    char digits[24];
    int n = 0;
    do {
        digits[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    while (n)
        *at++ = digits[--n];
    return at;
}

static char *put_str(char *at, const char *s)
{
    while (*s)
        *at++ = *s++;
    return at;
}

/* Write the ring out and empty it. The caller holds rec_lock. */
static void rec_flush_locked(void)
{
    if (rec_fd < 0) {
        ring_used = 0;
        return;
    }
    char buf[8192];
    char *at = buf;
    if (!header_written) {
        at = put_str(at, "framerec 1 pid ");
        at = put_u64(at, (uint64_t)getpid());
        *at++ = '\n';
        header_written = 1;
    }
    for (unsigned i = 0; i < ring_used; i++) {
        const struct rec *r = &ring[i];
        switch (r->what) {
        case REC_SWAP:
            at = put_str(at, "s ");
            break;
        case REC_STALL:
            at = put_str(at, "x ");
            break;
        case REC_NOTIFY:
            at = put_str(at, "c ");
            break;
        case REC_PIXMAP:
            at = put_str(at, "c ");
            break;
        default:
            at = put_str(at, "e ");
            break;
        }
        at = put_u64(at, r->serial);
        if (r->what == REC_NOTIFY)
            at = put_str(at, " n");
        else if (r->what == REC_PIXMAP)
            at = put_str(at, " p");
        *at++ = ' ';
        at = put_u64(at, r->a);
        if (r->what != REC_STALL && r->what != REC_ERROR) {
            *at++ = ' ';
            at = put_u64(at, r->b);
        }
        *at++ = '\n';
        if (at - buf > (long)sizeof buf - 128) {
            if (write(rec_fd, buf, (size_t)(at - buf))) { }
            at = buf;
        }
    }
    if (at > buf && write(rec_fd, buf, (size_t)(at - buf))) { }
    ring_used = 0;
}

static void lock(void)
{
    while (__sync_lock_test_and_set(&rec_lock, 1))
        ;
}

static void unlock(void)
{
    __sync_lock_release(&rec_lock);
}

/* Add one event; the caller holds rec_lock. */
static void rec_add(uint32_t what, uint32_t serial, uint64_t a, uint64_t b)
{
    if (rec_fd < 0)
        return;
    if (ring_used == RING)
        rec_flush_locked();
    ring[ring_used].what = what;
    ring[ring_used].serial = serial;
    ring[ring_used].a = a;
    ring[ring_used].b = b;
    ring_used++;
}

/* ---- Present, on a connection of the recorder's own ----------------- */

typedef struct { unsigned int sequence; } cookie_t;
typedef struct {
    uint8_t response_type;
    uint8_t pad0;
    uint16_t sequence;
    uint32_t pad[7];
    uint32_t full_sequence;
} generic_event_t;
typedef struct {
    uint8_t response_type;
    uint8_t pad0;
    uint16_t sequence;
    uint32_t length;
    uint8_t present;
    uint8_t major_opcode;
    uint8_t first_event;
    uint8_t first_error;
} extension_reply_t;

static struct {
    void *(*connect)(const char *, int *);
    int (*has_error)(void *);
    uint32_t (*generate_id)(void *);
    int (*flush)(void *);
    generic_event_t *(*poll_for_event)(void *);
    const extension_reply_t *(*get_extension_data)(void *, void *);
    int (*get_file_descriptor)(void *);
    cookie_t (*select_input)(void *, uint32_t, uint32_t, uint32_t);
    cookie_t (*notify_msc)(void *, uint32_t, uint32_t, uint64_t, uint64_t, uint64_t);
    void *present_id;
} xcb;

static void *conn;           /* the recorder's X connection, or 0 */
static int present_state;    /* 0 untried, 1 working, -1 off */
static uint8_t present_opcode;
static unsigned long watched; /* the window whose completions are selected */
static unsigned long notified, answered;

/* The bit the recorder's NotifyMSC serials carry, to tell them from the
 * program's own (Mesa numbers its swaps from 1). */
#define OURS 0x80000000u

/* GLX windows made by glXCreateWindow, by their GLX id: Present needs the
 * X window under them. */
#define GLX_WINDOWS 16
static unsigned long glx_ids[GLX_WINDOWS], x_windows[GLX_WINDOWS];

static unsigned long x_window_of(unsigned long drawable)
{
    for (int i = 0; i < GLX_WINDOWS; i++)
        if (glx_ids[i] == drawable)
            return x_windows[i];
    return drawable;
}

static int present_open(void)
{
    if (present_state)
        return present_state > 0;
    present_state = -1;
    if (rec_fd < 0)
        return 0;
    find_dlsym();
    void *lib = dlopen("libxcb.so.1", RTLD_NOW | RTLD_LOCAL);
    void *plib = dlopen("libxcb-present.so.0", RTLD_NOW | RTLD_LOCAL);
    if (!lib || !plib) {
        rec_add(REC_ERROR, 1, 0, 0); /* e 1: no libxcb or libxcb-present */
        return 0;
    }
    *(void **)&xcb.connect = real_dlsym(lib, "xcb_connect");
    *(void **)&xcb.has_error = real_dlsym(lib, "xcb_connection_has_error");
    *(void **)&xcb.generate_id = real_dlsym(lib, "xcb_generate_id");
    *(void **)&xcb.flush = real_dlsym(lib, "xcb_flush");
    *(void **)&xcb.poll_for_event = real_dlsym(lib, "xcb_poll_for_event");
    *(void **)&xcb.get_extension_data = real_dlsym(lib, "xcb_get_extension_data");
    *(void **)&xcb.get_file_descriptor = real_dlsym(lib, "xcb_get_file_descriptor");
    *(void **)&xcb.select_input = real_dlsym(plib, "xcb_present_select_input");
    *(void **)&xcb.notify_msc = real_dlsym(plib, "xcb_present_notify_msc");
    xcb.present_id = real_dlsym(plib, "xcb_present_id");
    if (!xcb.connect || !xcb.has_error || !xcb.generate_id || !xcb.flush ||
        !xcb.poll_for_event || !xcb.get_extension_data || !xcb.get_file_descriptor ||
        !xcb.select_input || !xcb.notify_msc || !xcb.present_id) {
        rec_add(REC_ERROR, 2, 0, 0); /* e 2: a function is missing */
        return 0;
    }
    void *c = xcb.connect(NULL, NULL);
    if (!c || xcb.has_error(c)) {
        rec_add(REC_ERROR, 3, 0, 0); /* e 3: no connection to $DISPLAY */
        return 0;
    }
    const extension_reply_t *ext = xcb.get_extension_data(c, xcb.present_id);
    if (!ext || !ext->present) {
        rec_add(REC_ERROR, 4, 0, 0); /* e 4: the server has no Present */
        return 0;
    }
    present_opcode = ext->major_opcode;
    conn = c;
    present_state = 1;
    return 1;
}

static uint64_t get_u64(const uint8_t *at)
{
    uint64_t v;
    memcpy(&v, at, sizeof v);
    return v;
}

static uint32_t get_u32(const uint8_t *at)
{
    uint32_t v;
    memcpy(&v, at, sizeof v);
    return v;
}

/* Take what the server has sent; the caller holds rec_lock. */
static void present_drain(void)
{
    generic_event_t *ev;
    while ((ev = xcb.poll_for_event(conn))) {
        const uint8_t *b = (const uint8_t *)ev;
        if (b[0] == 0) {
            rec_add(REC_ERROR, 5, b[1], 0); /* e 5 <code>: an X error */
        } else if ((b[0] & 0x7f) == 35 && b[1] == present_opcode &&
                   (b[8] | b[9] << 8) == 1 /* CompleteNotify */) {
            uint8_t kind = b[10];
            uint32_t serial = get_u32(b + 20);
            uint64_t ust = get_u64(b + 24), msc = get_u64(b + 36);
            if (kind == 1) {
                /* The server sends every NotifyMSC's completion to every
                 * client that selected them on the window, so the
                 * program's own come here too: ours carry OURS. */
                if (serial & OURS) {
                    answered++;
                    rec_add(REC_NOTIFY, serial & ~OURS, ust, msc);
                }
            } else {
                rec_add(REC_PIXMAP, serial, ust, msc);
            }
        }
        free(ev);
    }
}

/* After a swap of `drawable`: ask for its window's msc. */
static void present_after_swap(unsigned long drawable, uint32_t swap)
{
    if (!present_open())
        return;
    unsigned long window = x_window_of(drawable);
    if (window != watched) {
        xcb.select_input(conn, xcb.generate_id(conn), (uint32_t)window,
                         2 /* PresentCompleteNotifyMask */);
        watched = window;
    }
    xcb.notify_msc(conn, (uint32_t)window, OURS | swap, 0, 0, 0);
    notified++;
    xcb.flush(conn);
    present_drain();
    if (xcb.has_error(conn)) {
        rec_add(REC_ERROR, 6, 0, 0); /* e 6: the connection broke */
        present_state = -1;
    }
}

/* At the end: wait a little for the answers still owed. */
static void present_finish(void)
{
    if (present_state <= 0)
        return;
    uint64_t until = now_ns() + 500000000u;
    while (answered < notified && now_ns() < until && !xcb.has_error(conn)) {
        struct pollfd p = {xcb.get_file_descriptor(conn), POLLIN, 0};
        poll(&p, 1, 50);
        present_drain();
    }
}

/* ---- Swaps ---------------------------------------------------------- */

static uint32_t swaps;
static uint64_t stall_ns, stall_every_ns, next_stall;
static int stall_read;

/* A decimal number from the environment, or `otherwise`; not strtoul,
 * which glibc 2.38's headers turn into a symbol older ones lack. */
static uint64_t decimal(const char *s, uint64_t otherwise)
{
    if (!s || *s < '0' || *s > '9')
        return otherwise;
    uint64_t v = 0;
    while (*s >= '0' && *s <= '9')
        v = v * 10 + (uint64_t)(*s++ - '0');
    return v;
}

static void stall_maybe(uint32_t swap)
{
    if (!stall_read) {
        stall_read = 1;
        const char *ms = getenv("FRAMEREC_STALL_MS");
        const char *every = getenv("FRAMEREC_STALL_EVERY");
        stall_ns = decimal(ms, 0) * 1000000u;
        stall_every_ns = decimal(every, 10) * 1000000000u;
    }
    if (!stall_ns || !stall_every_ns)
        return;
    uint64_t now = now_ns();
    if (!next_stall) {
        next_stall = now + stall_every_ns;
        return;
    }
    if (now < next_stall)
        return;
    next_stall += stall_every_ns;
    struct timespec ts = {(time_t)(stall_ns / 1000000000u), (long)(stall_ns % 1000000000u)};
    while (nanosleep(&ts, &ts))
        ;
    lock();
    rec_add(REC_STALL, swap, stall_ns / 1000000u, 0);
    unlock();
}

static void on_term(int sig);

static uint32_t swap_begin(void)
{
    lock();
    if (rec_fd == -2) {
        rec_open();
        if (rec_fd >= 0) {
            struct sigaction sa, old;
            memset(&sa, 0, sizeof sa);
            sa.sa_handler = on_term;
            if (sigaction(SIGTERM, NULL, &old) == 0 && old.sa_handler == SIG_DFL)
                sigaction(SIGTERM, &sa, NULL);
        }
    }
    uint32_t swap = swaps++;
    unlock();
    stall_maybe(swap);
    return swap;
}

static void swap_end(uint32_t swap, uint64_t entry, unsigned long drawable)
{
    uint64_t back = now_ns();
    lock();
    rec_add(REC_SWAP, swap, entry, back);
    if (drawable)
        present_after_swap(drawable, swap);
    unlock();
}

static void recorded_glx_swap(void *dpy, unsigned long drawable)
{
    uint32_t swap = swap_begin();
    uint64_t entry = now_ns();
    real_glx_swap(dpy, drawable);
    swap_end(swap, entry, drawable);
    __sync_fetch_and_add(&n_glx, 1);
    tick();
}

static unsigned recorded_egl_swap(void *dpy, void *surface)
{
    uint32_t swap = swap_begin();
    uint64_t entry = now_ns();
    unsigned r = real_egl_swap(dpy, surface);
    swap_end(swap, entry, 0);
    __sync_fetch_and_add(&n_egl, 1);
    tick();
    return r;
}

static unsigned long recorded_glx_create_window(void *dpy, void *config, unsigned long win,
                                                const int *attribs)
{
    unsigned long id = real_glx_create_window(dpy, config, win, attribs);
    lock();
    for (int i = 0; i < GLX_WINDOWS; i++) {
        if (!glx_ids[i]) {
            glx_ids[i] = id;
            x_windows[i] = win;
            break;
        }
    }
    unlock();
    return id;
}

static void finish(void)
{
    lock();
    if (rec_fd >= 0) {
        present_finish();
        rec_flush_locked();
    }
    unlock();
}

__attribute__((destructor)) static void at_exit(void)
{
    finish();
}

static void on_term(int sig)
{
    /* Write what the ring holds, unless a swap is halfway through adding to
     * it, then die of the signal as the program would have. */
    if (!__sync_lock_test_and_set(&rec_lock, 1)) {
        rec_flush_locked();
        __sync_lock_release(&rec_lock);
    }
    signal(sig, SIG_DFL);
    raise(sig);
}

/* ---- XInput2 counts, as swapfps had them ---------------------------- */

/* XGenericEventCookie, without Xlib's headers. */
struct cookie {
    int type;
    unsigned long serial;
    int send_event;
    void *display;
    int extension;
    int evtype;
    unsigned int cookie;
    void *data;
};

static int counted_get_event_data(void *dpy, void *ev)
{
    int r = real_get_event_data(dpy, ev);
    if (r) {
        int t = ((struct cookie *)ev)->evtype;
        if (t == 17) /* XI_RawMotion */
            __sync_fetch_and_add(&n_raw, 1);
        else if (t == 6) /* XI_Motion */
            __sync_fetch_and_add(&n_motion, 1);
    }
    return r;
}

/* ---- Handing out the wrappers --------------------------------------- */

static void *(*real_gpa)(const char *);
static void *(*real_gpa_arb)(const char *);

/* The wrapper for `name`, remembering `real` as what it calls; or `real`
 * when the name is not one of those wrapped. */
static void *wrap(void *real, const char *name)
{
    if (!real)
        return real;
#define WRAP(fn, wrapper, store)                                                         \
    if (!strcmp(name, fn)) {                                                             \
        if (real != (void *)(wrapper))                                                   \
            *(void **)&(store) = real;                                                   \
        return (void *)(wrapper);                                                        \
    }
    WRAP("glXSwapBuffers", recorded_glx_swap, real_glx_swap)
    WRAP("glXCreateWindow", recorded_glx_create_window, real_glx_create_window)
    WRAP("eglSwapBuffers", recorded_egl_swap, real_egl_swap)
    WRAP("XGetEventData", counted_get_event_data, real_get_event_data)
#undef WRAP
    return real;
}

static void *recorded_gpa(const char *name)
{
    return wrap(real_gpa(name), name);
}

static void *recorded_gpa_arb(const char *name)
{
    return wrap(real_gpa_arb(name), name);
}

void *dlsym(void *handle, const char *name)
{
    find_dlsym();
    void *real = real_dlsym(handle, name);
    if (!real)
        return real;
    if (!strcmp(name, "glXGetProcAddress")) {
        if (real != (void *)recorded_gpa)
            real_gpa = real;
        return (void *)recorded_gpa;
    }
    if (!strcmp(name, "glXGetProcAddressARB")) {
        if (real != (void *)recorded_gpa_arb)
            real_gpa_arb = real;
        return (void *)recorded_gpa_arb;
    }
    return wrap(real, name);
}

/* For programs that link the functions instead of looking them up. */
#define NEXT(store, fn)                                                                  \
    if (!(store)) {                                                                      \
        find_dlsym();                                                                    \
        *(void **)&(store) = real_dlsym(RTLD_NEXT, fn);                                  \
    }

void glXSwapBuffers(void *dpy, unsigned long drawable)
{
    NEXT(real_glx_swap, "glXSwapBuffers")
    if (real_glx_swap)
        recorded_glx_swap(dpy, drawable);
}

unsigned long glXCreateWindow(void *dpy, void *config, unsigned long win, const int *attribs)
{
    NEXT(real_glx_create_window, "glXCreateWindow")
    return real_glx_create_window ? recorded_glx_create_window(dpy, config, win, attribs) : 0;
}

unsigned eglSwapBuffers(void *dpy, void *surface)
{
    NEXT(real_egl_swap, "eglSwapBuffers")
    return real_egl_swap ? recorded_egl_swap(dpy, surface) : 0;
}

int XGetEventData(void *dpy, void *ev)
{
    NEXT(real_get_event_data, "XGetEventData")
    return real_get_event_data ? counted_get_event_data(dpy, ev) : 0;
}

void *glXGetProcAddress(const char *name)
{
    NEXT(real_gpa, "glXGetProcAddress")
    return real_gpa ? recorded_gpa(name) : 0;
}

void *glXGetProcAddressARB(const char *name)
{
    NEXT(real_gpa_arb, "glXGetProcAddressARB")
    return real_gpa_arb ? recorded_gpa_arb(name) : 0;
}
