/* Compile check of board-bench.h, and a run where the kernel gives user
 * mode the PMU: prints the clock, the timer floor and a touch sweep.
 *   arm-linux-gnueabihf-gcc -O2 -static -Wall -Wextra -o selftest selftest.c */
#include <stdlib.h>

#include "board-bench.h"

static struct bb_series s;
static char buf[256 * 4096] __attribute__((aligned(4096)));

int main(void)
{
    if (bb_pmu_start() != 0) {
        BB_PRINTF("selftest error pmu-user-access-off\n");
        return 1;
    }
    uint64_t hz = bb_cpu_hz();
    bb_print_clock("selftest", hz);
    BB_RUN(&s, (void)0);
    bb_print("selftest", "timer-floor", &s, hz);
    BB_RUN(&s, bb_touch(buf, 64, 4096));
    bb_print("selftest", "touch-64-pages", &s, hz);
    return 0;
}
