/* framegl: the frame recorder's test client (docs/GAME-PATH.md §2.4, R1).
 *
 * A GLX window that clears to a changing colour and swaps, for a number of
 * seconds, paced to the X server's refresh through the Present extension:
 * after each swap it asks for the window's msc and waits for the next one,
 * as a driver with vsync on waits for the vertical blank, with at most one
 * frame in flight. A frame that takes longer than a refresh therefore
 * shows at the next refresh after it is done, whatever way GLX puts it on
 * the screen -- llvmpipe's XPutImage has no vsync of its own.
 *
 *     framegl <seconds> [free] [<width> <height>]
 *
 * `free` swaps as fast as it can, unpaced. Every line it prints starts
 * `framegl:`. It ends with its frames, the msc it started and ended at, and
 * GL's renderer. Its exit status is 0 when it drew for the whole time.
 *
 * libX11, libGL, libxcb and libxcb-present are opened at run time, so it
 * builds with nothing but a C compiler and the C library:
 *   cc -O2 -o framegl framegl.c -ldl
 * and glXSwapBuffers is looked up with dlsym, as SDL2 looks it up: the road
 * the recorder must wrap for SDL2's games.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* Xlib's XSetWindowAttributes and XVisualInfo, without its headers. */
struct set_window_attributes {
    unsigned long background_pixmap, background_pixel, border_pixmap, border_pixel;
    int bit_gravity, win_gravity, backing_store;
    unsigned long backing_planes, backing_pixel;
    int save_under;
    long event_mask, do_not_propagate_mask;
    int override_redirect;
    unsigned long colormap, cursor;
};
struct visual_info {
    void *visual;
    unsigned long visualid;
    int screen, depth;
};
typedef struct { unsigned int sequence; } cookie_t;
typedef struct {
    uint8_t response_type, pad0;
    uint16_t sequence;
    uint32_t length;
    uint8_t present, major_opcode, first_event, first_error;
} extension_reply_t;

static void *open_lib(const char *name)
{
    void *lib = dlopen(name, RTLD_NOW | RTLD_GLOBAL);
    if (!lib) {
        printf("framegl: cannot open %s: %s\n", name, dlerror());
        exit(2);
    }
    return lib;
}

static void *need(void *lib, const char *name)
{
    void *fn = dlsym(lib, name);
    if (!fn) {
        printf("framegl: no %s\n", name);
        exit(2);
    }
    return fn;
}

#define FN(lib, ret, name, ...) ret (*name)(__VA_ARGS__) = need(lib, #name)

static uint64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000;
}

/* The Present connection: wait for the completion of our NotifyMSC with
 * `serial` and give its msc. Others' completions (the recorder's) are
 * passed over. */
static void *xc;
static uint8_t opcode;
static void *(*wait_for_event)(void *);
static cookie_t (*notify_msc)(void *, uint32_t, uint32_t, uint64_t, uint64_t, uint64_t);
static int (*xflush)(void *);

static uint64_t msc_of(uint32_t window, uint32_t serial, uint64_t target)
{
    notify_msc(xc, window, serial, target, 0, 0);
    xflush(xc);
    for (;;) {
        uint8_t *b = wait_for_event(xc);
        if (!b) {
            printf("framegl: the Present connection broke\n");
            exit(3);
        }
        if (b[0] == 0) {
            printf("framegl: X error %u on the Present connection\n", b[1]);
            exit(3);
        }
        uint32_t got;
        uint64_t msc;
        memcpy(&got, b + 20, 4);
        memcpy(&msc, b + 36, 8);
        int ours = (b[0] & 0x7f) == 35 && b[1] == opcode && b[8] == 1 && b[10] == 1 &&
                   got == serial;
        free(b);
        if (ours)
            return msc;
    }
}

int main(int argc, char **argv)
{
    int seconds = argc > 1 ? atoi(argv[1]) : 10;
    int paced = !(argc > 2 && !strcmp(argv[2], "free"));
    int arg = paced ? 2 : 3;
    unsigned width = argc > arg + 1 ? (unsigned)atoi(argv[arg]) : 320;
    unsigned height = argc > arg + 1 ? (unsigned)atoi(argv[arg + 1]) : 240;
    setvbuf(stdout, NULL, _IOLBF, 0);

    void *x11 = open_lib("libX11.so.6");
    void *gl = open_lib("libGL.so.1");
    void *xcb = open_lib("libxcb.so.1");
    void *xcbp = open_lib("libxcb-present.so.0");
    FN(x11, void *, XOpenDisplay, const char *);
    FN(x11, int, XDefaultScreen, void *);
    FN(x11, unsigned long, XRootWindow, void *, int);
    FN(x11, unsigned long, XCreateColormap, void *, unsigned long, void *, int);
    FN(x11, unsigned long, XCreateWindow, void *, unsigned long, int, int, unsigned, unsigned,
       unsigned, int, unsigned, void *, unsigned long, struct set_window_attributes *);
    FN(x11, int, XStoreName, void *, unsigned long, const char *);
    FN(x11, int, XSetClassHint, void *, unsigned long, char **);
    FN(x11, int, XMapWindow, void *, unsigned long);
    FN(x11, int, XSync, void *, int);
    FN(gl, struct visual_info *, glXChooseVisual, void *, int, int *);
    FN(gl, void *, glXCreateContext, void *, struct visual_info *, void *, int);
    FN(gl, int, glXMakeCurrent, void *, unsigned long, void *);
    FN(gl, void, glXSwapBuffers, void *, unsigned long);
    FN(gl, void, glClearColor, float, float, float, float);
    FN(gl, void, glClear, unsigned);
    FN(gl, const char *, glGetString, unsigned);
    FN(xcb, void *, xcb_connect, const char *, int *);
    FN(xcb, int, xcb_connection_has_error, void *);
    FN(xcb, uint32_t, xcb_generate_id, void *);
    FN(xcb, const extension_reply_t *, xcb_get_extension_data, void *, void *);
    FN(xcbp, cookie_t, xcb_present_select_input, void *, uint32_t, uint32_t, uint32_t);
    void *present_id = need(xcbp, "xcb_present_id");
    *(void **)&wait_for_event = need(xcb, "xcb_wait_for_event");
    *(void **)&xflush = need(xcb, "xcb_flush");
    *(void **)&notify_msc = need(xcbp, "xcb_present_notify_msc");

    void *dpy = XOpenDisplay(NULL);
    if (!dpy) {
        printf("framegl: cannot open the display\n");
        return 2;
    }
    int screen = XDefaultScreen(dpy);
    unsigned long root = XRootWindow(dpy, screen);
    /* GLX_RGBA, GLX_DOUBLEBUFFER, GLX_RED/GREEN/BLUE_SIZE 1, None */
    int attribs[] = {4, 5, 8, 1, 9, 1, 10, 1, 0};
    struct visual_info *vi = glXChooseVisual(dpy, screen, attribs);
    if (!vi) {
        printf("framegl: no double-buffered RGBA visual\n");
        return 2;
    }
    struct set_window_attributes swa;
    memset(&swa, 0, sizeof swa);
    swa.colormap = XCreateColormap(dpy, root, vi->visual, 0 /* AllocNone */);
    unsigned long window = XCreateWindow(dpy, root, 0, 0, width, height, 0, vi->depth,
                                         1 /* InputOutput */, vi->visual,
                                         (1ul << 3) | (1ul << 13) /* BorderPixel, Colormap */,
                                         &swa);
    XStoreName(dpy, window, "framegl");
    char name[] = "framegl";
    char *class_hint[2] = {name, name}; /* XClassHint: res_name, res_class */
    XSetClassHint(dpy, window, class_hint);
    XMapWindow(dpy, window);
    XSync(dpy, 0);

    xc = xcb_connect(NULL, NULL);
    const extension_reply_t *ext =
        xc && !xcb_connection_has_error(xc) ? xcb_get_extension_data(xc, present_id) : NULL;
    if (!ext || !ext->present) {
        printf("framegl: the server has no Present extension\n");
        return 2;
    }
    opcode = ext->major_opcode;
    xcb_present_select_input(xc, xcb_generate_id(xc), (uint32_t)window,
                             2 /* PresentCompleteNotifyMask */);
    uint32_t serial = 1;
    uint64_t first_msc = msc_of((uint32_t)window, serial++, 0), last_msc = first_msc;
    printf("framegl: msc %llu at the start\n", (unsigned long long)first_msc);

    void *ctx = glXCreateContext(dpy, vi, NULL, 1);
    if (!ctx || !glXMakeCurrent(dpy, window, ctx)) {
        printf("framegl: no GL context\n");
        return 2;
    }
    const char *renderer = glGetString(0x1F01 /* GL_RENDERER */);
    printf("framegl: renderer %s, %ux%u, %s\n", renderer ? renderer : "?", width, height,
           paced ? "paced to the msc" : "free");

    uint64_t start = now_ms(), end = start + (uint64_t)seconds * 1000;
    unsigned long frames = 0, unpaced = 0;
    while (now_ms() < end) {
        float t = (float)(frames % 120) / 120.0f;
        glClearColor(t, 1.0f - t, 0.5f, 1.0f);
        glClear(0x4000 /* GL_COLOR_BUFFER_BIT */);
        glXSwapBuffers(dpy, window);
        frames++;
        if (paced) {
            /* The msc now, then the first later one. Xorg answers a
             * NotifyMSC for now + 1 at the next refresh; Xwayland one
             * frame early, since its copies complete a frame after they
             * run (xwl_present_get_exec_msc), so it is asked again for
             * now + 2. A server whose msc never moves answers each at
             * once: after three tries the frame goes unpaced. */
            uint64_t now = msc_of((uint32_t)window, serial++, 0);
            uint64_t target = now + 1;
            while ((last_msc = msc_of((uint32_t)window, serial++, target)) <= now &&
                   target < now + 3)
                target++;
            if (last_msc <= now && !unpaced++)
                printf("framegl: the msc did not move past %llu; this frame and any like it "
                       "are unpaced\n",
                       (unsigned long long)now);
        }
        if (serial > 0x40000000u)
            serial = 1;
    }
    if (!paced)
        last_msc = msc_of((uint32_t)window, serial++, 0);
    uint64_t took = now_ms() - start;
    printf("framegl: frames %lu in %llu ms, msc %llu to %llu (%llu refreshes), unpaced %lu\n",
           frames, (unsigned long long)took, (unsigned long long)first_msc,
           (unsigned long long)last_msc, (unsigned long long)(last_msc - first_msc), unpaced);
    return 0;
}
