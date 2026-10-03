/*
 * nvos.h: the C view of ferrix-nvos (os/nvos/), the Rust half of nvrm's OS
 * layer (docs/NVIDIA.md §4.2).
 *
 * RM's own os_* calls are declared by NVIDIA's os-interface.h. This header
 * declares what ferrix-nvos gives NVIDIA's kept C and Ferrix's C glue to be
 * rebuilt on: locks they can embed, timers, work queues, pinned pages, the
 * device, client requests and lines on the console. Each lock type is the
 * Rust type's words, laid out the same; zero is a free mutex, a free
 * reader-writer lock, an empty semaphore, a pending completion and a fresh
 * event.
 *
 * SPDX-License-Identifier: MIT
 */
#ifndef NVOS_H
#define NVOS_H

#include <nvtypes.h>
#include <nvstatus.h>

/* sync::Mutex: also RM's spinlocks, taken after a spin. */
typedef struct { NvU32 state; } nvos_mutex_t;
/* sync::Semaphore: Linux's struct semaphore. */
typedef struct { NvU32 count; NvU32 sleepers; } nvos_sema_t;
/* sync::RwLock. */
typedef struct { NvU32 state; NvU32 writers_waiting; NvU32 sequence; } nvos_rwlock_t;
/* sync::Completion: done once, done for good (complete_all). */
typedef struct { NvU32 done; } nvos_completion_t;
/* sync::Event: a sequence a sleeper waits on to move. */
typedef struct { NvU32 sequence; } nvos_event_t;

void nvos_mutex_lock(nvos_mutex_t *);
NvBool nvos_mutex_trylock(nvos_mutex_t *);
void nvos_mutex_unlock(nvos_mutex_t *);
void nvos_spin_lock(nvos_mutex_t *);
#define nvos_spin_unlock nvos_mutex_unlock

void nvos_sema_init(nvos_sema_t *, NvU32 count);
void nvos_sema_down(nvos_sema_t *);
NvBool nvos_sema_trydown(nvos_sema_t *);
void nvos_sema_up(nvos_sema_t *);

void nvos_rwlock_read(nvos_rwlock_t *);
void nvos_rwlock_write(nvos_rwlock_t *);
void nvos_rwlock_read_unlock(nvos_rwlock_t *);
void nvos_rwlock_write_unlock(nvos_rwlock_t *);

void nvos_completion_wait(nvos_completion_t *);
void nvos_completion_complete_all(nvos_completion_t *);

NvU32 nvos_event_now(nvos_event_t *);
void nvos_event_wait(nvos_event_t *, NvU32 seen);
void nvos_event_signal(nvos_event_t *);

/* timer.rs: one thread runs every callback at its deadline. */
struct nvos_timer;
struct nvos_timer *nvos_timer_create(void (*callback)(void *), void *argument);
void nvos_timer_start(struct nvos_timer *, NvU64 nanoseconds);
NvBool nvos_timer_cancel(struct nvos_timer *);
void nvos_timer_destroy(struct nvos_timer *);

/* workq.rs: a thread per queue; NULL is the global queue. */
void *nvos_work_queue_create(void);
NvBool nvos_work_queue_schedule(void *queue, void (*run)(void *), void *argument);
void nvos_work_queue_flush(void *queue);

/* pages.rs: system memory, pinned into the device's domain when attached. */
struct nvos_pages;
NV_STATUS nvos_pages_alloc(NvU64 count, NvBool contiguous, NvU64 *addresses,
                           void **mapped, struct nvos_pages **pages);
void nvos_pages_free(struct nvos_pages *);

/* device.rs: the GPU, once nvrm attaches its handle. */
NV_STATUS nvos_device_attach(NvU32 handle);
NV_STATUS nvos_interrupt_start(NvU32 index, void (*handler)(void *), void *argument);

/* The attached device, for nvrm's probe (device.rs's DeviceDesc). */
#define NVOS_APERTURES 16
#define NVOS_APERTURE_PREFETCHABLE 0x1
struct nvos_device_desc {
    NvU32 location;     /* segment 31:16, bus 15:8, devfn 7:0 */
    NvU32 class_code;   /* base 23:16, subclass 15:8 */
    NvU16 vendor;
    NvU16 device;
    NvU32 vectors;
    NvU32 apertures;
    NvU8 bar[NVOS_APERTURES];
    NvU8 flags[NVOS_APERTURES];
    NvU64 phys[NVOS_APERTURES];
    NvU64 len[NVOS_APERTURES];
};
NV_STATUS nvos_device_describe(struct nvos_device_desc *out);

/* thread.rs: a detached thread, for the chardev dispatcher's workers. */
NvBool nvos_thread_spawn(void (*run)(void *), void *argument);

/* chardev.rs: the request bridge to the kernel's chardev core (N1e). */
struct nvos_request {
    NvU64 id;
    NvU64 file;
    NvU32 op;           /* 1 open, 2 ioctl, 3 release */
    NvU32 minor;
    NvU32 pid;
    NvU32 euid;
    NvU32 egid;
    NvU32 cmd;
    NvU64 arg;
};
#define NVOS_REQUEST_OPEN    1
#define NVOS_REQUEST_IOCTL   2
#define NVOS_REQUEST_RELEASE 3
NV_STATUS nvos_chardev_start(const NvU16 *minors, NvU32 count);
NV_STATUS nvos_chardev_next(struct nvos_request *out);
int nvos_chardev_reply(NvU64 id, int status, NvS64 value);
int nvos_chardev_copy_in(NvU64 id, void *to, NvU64 from, NvU32 length);
int nvos_chardev_copy_out(NvU64 id, NvU64 to, const void *from, NvU32 length);
NvS64 nvos_chardev_file(NvU64 id, int fd);
void nvos_isr_enter_leave(NvBool entering);

/* client.rs: the client the calling thread serves a request for. */
typedef struct nvos_client {
    NvU32 pid;
    NvU32 euid;
    NvBool administrator;
    char name[16];
    int (*copy_in)(void *context, void *to, NvU64 from, NvU32 length);
    int (*copy_out)(void *context, NvU64 to, const void *from, NvU32 length);
    void *context;
    NvS64 (*resolve_fd)(void *context, int fd);
} nvos_client_t;
NvBool nvos_client_enter(const nvos_client_t *);
void nvos_client_leave(void);
NvS64 nvos_client_resolve_fd(int fd);

/* os.rs and log.rs. */
void *nvos_read_whole_file(const char *path, NvU64 *size);
void nvos_write_line(const char *text);

#endif
