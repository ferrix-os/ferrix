/*
 * thread-nice: a nice value is a thread's, as on Linux. `cargo xtask
 * test-shader-cache` runs it before the shader caches (docs/WINE.md §6).
 *
 * A thread lowers itself to 19 with setpriority(PRIO_PROCESS, its tid, 19),
 * as Mesa's disk-cache threads do. Then the program reads back that thread's
 * value, the main thread's, and the value of a thread the main thread starts
 * afterwards, and prints one line:
 *
 *   thread-nice: worker=19 main=0 later=0
 *
 * which is what Linux prints. A kernel that keeps the value per process
 * prints main=19 later=19: every thread of a program that uses Mesa then
 * runs at the lowest weight beside every other program.
 *
 * Built static by xtask with the host's compiler:
 *   cc -O2 -Wall -Wextra -Werror -static -pthread -o thread-nice thread-nice.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

static int worker_nice = -100, later_nice = -100;

/* This thread's nice value, or -100 when getpriority fails. */
static int own_nice(void) {
    pid_t tid = (pid_t)syscall(SYS_gettid);
    errno = 0;
    int nice = getpriority(PRIO_PROCESS, (id_t)tid);
    return nice == -1 && errno != 0 ? -100 : nice;
}

static void *worker(void *unused) {
    (void)unused;
    pid_t tid = (pid_t)syscall(SYS_gettid);
    if (setpriority(PRIO_PROCESS, (id_t)tid, 19) != 0)
        printf("thread-nice: setpriority(PRIO_PROCESS, %d, 19) failed: errno %d\n", (int)tid,
               errno);
    worker_nice = own_nice();
    return NULL;
}

static void *later(void *unused) {
    (void)unused;
    later_nice = own_nice();
    return NULL;
}

int main(void) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL) || pthread_join(thread, NULL))
        return 1;
    int main_nice = own_nice();
    if (pthread_create(&thread, NULL, later, NULL) || pthread_join(thread, NULL))
        return 1;
    printf("thread-nice: worker=%d main=%d later=%d\n", worker_nice, main_nice, later_nice);
    return 0;
}
