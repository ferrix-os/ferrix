/*
 * syncbench: the Windows program `cargo xtask test-wine-sync` runs in
 * Proton's Wine on Ferrix, with wineserver's synchronisation and with fsync
 * (stage 23, E1.4 and E3.5; docs/WINE.md §11). Built for x86-64 and for
 * i386, so that both of Proton's Unix sides wait.
 *
 *   syncbench.exe ROUNDS            the two benches below
 *   syncbench.exe child ROUNDS      the second process of the second bench
 *
 * Threads: two threads hand a turn back and forth ROUNDS times through two
 * auto-reset events, the waiting side in WaitForMultipleObjects on three
 * handles of which the event it waits for is the second. Processes: the
 * same between this process and a child it starts, through named events.
 * Each prints one line the gate reads:
 *
 *   syncbench: threads rounds=N ms=T us_per_round=U index=1
 *   syncbench: processes rounds=N ms=T us_per_round=U
 *
 * index is what the multiple wait answered (WAIT_OBJECT_0 + 1 every time,
 * or the first other answer). With wineserver alone every wait and every
 * SetEvent is a request to the server; with fsync both are a futex word in
 * shared memory and futex_waitv or FUTEX_WAKE, and the server is not asked.
 *
 * Built by xtask with mingw-w64 (crate::wine::windows_program):
 *   x86_64-w64-mingw32-gcc -O2 -o syncbench64.exe syncbench.c
 *   i686-w64-mingw32-gcc -O2 -o syncbench32.exe syncbench.c
 */

#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static HANDLE ping, pong, idle[2];
static long rounds;
static volatile DWORD bad_index = WAIT_OBJECT_0 + 1;

static double now_ms(void) {
    LARGE_INTEGER count, frequency;
    QueryPerformanceCounter(&count);
    QueryPerformanceFrequency(&frequency);
    return (double)count.QuadPart * 1000.0 / (double)frequency.QuadPart;
}

static DWORD wait_second(HANDLE wanted) {
    HANDLE handles[3] = {idle[0], wanted, idle[1]};
    DWORD index = WaitForMultipleObjects(3, handles, FALSE, 10000);
    if (index != WAIT_OBJECT_0 + 1 && bad_index == WAIT_OBJECT_0 + 1)
        bad_index = index;
    return index;
}

static DWORD WINAPI partner(void *unused) {
    long i;
    (void)unused;
    for (i = 0; i < rounds; i++) {
        if (wait_second(ping) != WAIT_OBJECT_0 + 1)
            return 1;
        SetEvent(pong);
    }
    return 0;
}

static int threads(void) {
    long i;
    double start, ms;
    HANDLE thread;

    ping = CreateEventA(NULL, FALSE, FALSE, NULL);
    pong = CreateEventA(NULL, FALSE, FALSE, NULL);
    idle[0] = CreateEventA(NULL, TRUE, FALSE, NULL);
    idle[1] = CreateEventA(NULL, TRUE, FALSE, NULL);
    thread = CreateThread(NULL, 0, partner, NULL, 0, NULL);
    if (!ping || !pong || !idle[0] || !idle[1] || !thread) {
        printf("syncbench: threads: could not make the events or the thread: %lu\n",
               GetLastError());
        return 1;
    }
    start = now_ms();
    for (i = 0; i < rounds; i++) {
        SetEvent(ping);
        if (wait_second(pong) != WAIT_OBJECT_0 + 1)
            break;
    }
    ms = now_ms() - start;
    WaitForSingleObject(thread, 10000);
    printf("syncbench: threads rounds=%ld ms=%.0f us_per_round=%.1f index=%lu\n", i, ms,
           i ? ms * 1000.0 / (double)i : 0.0, (unsigned long)(bad_index - WAIT_OBJECT_0));
    fflush(stdout);
    return i == rounds && bad_index == WAIT_OBJECT_0 + 1 ? 0 : 1;
}

static int child(void) {
    long i;
    ping = OpenEventA(EVENT_ALL_ACCESS, FALSE, "ferrix-syncbench-ping");
    pong = OpenEventA(EVENT_ALL_ACCESS, FALSE, "ferrix-syncbench-pong");
    if (!ping || !pong)
        return 3;
    for (i = 0; i < rounds; i++) {
        if (WaitForSingleObject(ping, 10000) != WAIT_OBJECT_0)
            return 4;
        SetEvent(pong);
    }
    return 0;
}

static int processes(const char *self) {
    char command[MAX_PATH + 64];
    STARTUPINFOA startup;
    PROCESS_INFORMATION started;
    long i;
    double start, ms;
    DWORD status = 99;

    ping = CreateEventA(NULL, FALSE, FALSE, "ferrix-syncbench-ping");
    pong = CreateEventA(NULL, FALSE, FALSE, "ferrix-syncbench-pong");
    if (!ping || !pong) {
        printf("syncbench: processes: could not make the events: %lu\n", GetLastError());
        return 1;
    }
    memset(&startup, 0, sizeof(startup));
    startup.cb = sizeof(startup);
    snprintf(command, sizeof(command), "\"%s\" child %ld", self, rounds);
    if (!CreateProcessA(NULL, command, NULL, NULL, FALSE, 0, NULL, NULL, &startup, &started)) {
        printf("syncbench: processes: could not start the child: %lu\n", GetLastError());
        return 1;
    }
    start = now_ms();
    for (i = 0; i < rounds; i++) {
        SetEvent(ping);
        if (WaitForSingleObject(pong, 10000) != WAIT_OBJECT_0)
            break;
    }
    ms = now_ms() - start;
    WaitForSingleObject(started.hProcess, 10000);
    GetExitCodeProcess(started.hProcess, &status);
    printf("syncbench: processes rounds=%ld ms=%.0f us_per_round=%.1f child=%lu\n", i, ms,
           i ? ms * 1000.0 / (double)i : 0.0, (unsigned long)status);
    fflush(stdout);
    return i == rounds && status == 0 ? 0 : 1;
}

int main(int argc, char **argv) {
    char self[MAX_PATH];
    int failed;

    if (argc == 3 && strcmp(argv[1], "child") == 0) {
        rounds = strtol(argv[2], NULL, 10);
        return child();
    }
    if (argc != 2) {
        printf("syncbench: usage: syncbench.exe ROUNDS\n");
        return 2;
    }
    rounds = strtol(argv[1], NULL, 10);
    GetModuleFileNameA(NULL, self, sizeof(self));
    printf("syncbench: pointer=%u\n", (unsigned)sizeof(void *));
    fflush(stdout);
    failed = threads();
    failed |= processes(self);
    return failed;
}
