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
} nvos_client_t;
NvBool nvos_client_enter(const nvos_client_t *);
void nvos_client_leave(void);

/* os.rs and log.rs. */
void *nvos_read_whole_file(const char *path, NvU64 *size);
void nvos_write_line(const char *text);

#endif
