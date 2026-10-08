/* board-bench.h: how every kernel on the DK1 is timed (docs/BOARD-BENCH.md).
 *
 * One header for the Linux and seL4 benches, and the contract Ferrix's
 * ipc-bench follows in Rust, so all three kernels are timed alike:
 *
 *  - Each sample reads the cycle counter (PMCCNTR), then the
 *    instructions-retired counter, then runs the operation, then reads
 *    instructions and cycles again, each read after an ISB. The `timer-floor`
 *    line times an empty operation, which is what the reads cost.
 *  - The counters count every mode (no P/U filter), so a round trip's kernel
 *    time is in its cycles.
 *  - WARMUP untimed operations come first, then SAMPLES timed ones. Every
 *    sample is kept and sorted. A percentile is sorted[n * per_mille / 1000],
 *    exactly as ipc-bench reads it.
 *  - ns come from cycles at the clock measured against the system counter
 *    (`clock` line), never from the system counter alone: at 24 MHz its tick
 *    is about 42 ns.
 *
 * Lines, one per series:
 *   <prog> clock cpu_hz=<hz> cntfrq=<hz>
 *   <prog> <what> n=<n> min=<ns> p50=<ns> p90=<ns> p99=<ns> mean=<ns>
 *   <prog> <what>.cycles n=<n> min= p50= p90= p99= mean=
 *   <prog> <what>.ins n=<n> min= p50= p90= p99= mean=
 *   <prog> <what>.cycles.pct v=<p0>,<p1>,...,<p100>
 * The first is the shape `bench-ipc --board-log` reads.
 *
 * ARMv7-A only. The kernel must set PMUSERENR.EN (and CNTKCTL.PL0VCTEN);
 * bb_pmu_start() refuses to run without them. The includer defines
 * BB_PRINTF if printf is not its output.
 */
#ifndef BOARD_BENCH_H
#define BOARD_BENCH_H

#include <stdint.h>

#if !defined(__arm__) || defined(__aarch64__)
#error "board-bench.h is for ARMv7-A"
#endif

#ifndef BB_PRINTF
#include <stdio.h>
#define BB_PRINTF printf
#endif

#define BB_WARMUP 1000u
#define BB_SAMPLES 20000u

/* Instructions architecturally executed (ARMv7 PMU event 0x08). */
#define BB_EVENT_INST_RETIRED 0x08u

static inline uint32_t bb_pmuserenr(void)
{
    uint32_t v;
    __asm__ volatile("mrc p15, 0, %0, c9, c14, 0" : "=r"(v));
    return v;
}

static inline uint32_t bb_cycles(void)
{
    uint32_t v;
    __asm__ volatile("isb\n\tmrc p15, 0, %0, c9, c13, 0" : "=r"(v) : : "memory");
    return v;
}

/* Event counter 0, selected once by bb_pmu_start(). */
static inline uint32_t bb_insns(void)
{
    uint32_t v;
    __asm__ volatile("isb\n\tmrc p15, 0, %0, c9, c13, 2" : "=r"(v) : : "memory");
    return v;
}

static inline uint64_t bb_cntvct(void)
{
    uint32_t lo, hi;
    __asm__ volatile("isb\n\tmrrc p15, 1, %0, %1, c14" : "=r"(lo), "=r"(hi) : : "memory");
    return ((uint64_t)hi << 32) | lo;
}

static inline uint32_t bb_cntfrq(void)
{
    uint32_t v;
    __asm__ volatile("mrc p15, 0, %0, c14, c0, 0" : "=r"(v));
    return v;
}

/* Start the cycle counter and event counter 0 (instructions), both reset,
 * no divider. Returns 0, or -1 when the kernel left user access off. */
static inline int bb_pmu_start(void)
{
    if ((bb_pmuserenr() & 1u) == 0)
        return -1;
    __asm__ volatile(
        "mcr p15, 0, %0, c9, c12, 5\n\t" /* PMSELR = 0 */
        "mcr p15, 0, %1, c9, c13, 1\n\t" /* PMXEVTYPER = INST_RETIRED */
        "mcr p15, 0, %2, c9, c12, 3\n\t" /* PMOVSR: clear overflows */
        "mcr p15, 0, %3, c9, c12, 0\n\t" /* PMCR = E | P | C, D = 0 */
        "mcr p15, 0, %4, c9, c12, 1\n\t" /* PMCNTENSET = cycles | counter 0 */
        "isb"
        :
        : "r"(0u), "r"(BB_EVENT_INST_RETIRED), "r"(0xffffffffu), "r"(0x7u),
          "r"((1u << 31) | 1u)
        : "memory");
    return 0;
}

/* The caller's own work between calls: one load from each of `n` (> 0)
 * addresses `stride` bytes apart from `base`, the same instructions on
 * every kernel whatever the compiler. Stride 4096 walks pages (TLB), 64
 * walks lines (caches). */
static inline void bb_touch(const void *base, uint32_t n, uint32_t stride)
{
    const void *p = base;
    __asm__ volatile(
        "1:\n\t"
        "ldr r12, [%0]\n\t"
        "add %0, %0, %2\n\t"
        "subs %1, %1, #1\n\t"
        "bne 1b"
        : "+r"(p), "+r"(n)
        : "r"(stride)
        : "r12", "cc", "memory");
}

/* One series: every sample kept. */
struct bb_series {
    uint32_t n;
    uint64_t cyc_sum, ins_sum;
    uint32_t cyc[BB_SAMPLES];
    uint32_t ins[BB_SAMPLES];
};

static inline void bb_reset(struct bb_series *s)
{
    s->n = 0;
    s->cyc_sum = 0;
    s->ins_sum = 0;
}

static inline void bb_add(struct bb_series *s, uint32_t cyc, uint32_t ins)
{
    if (s->n < BB_SAMPLES) {
        s->cyc[s->n] = cyc;
        s->ins[s->n] = ins;
        s->cyc_sum += cyc;
        s->ins_sum += ins;
        s->n++;
    }
}

/* Time one statement into a series: cycles, instructions, the statement,
 * instructions, cycles. */
#define BB_TIME(series, STMT)                                   \
    do {                                                        \
        uint32_t bb_c0 = bb_cycles(), bb_i0 = bb_insns();       \
        STMT;                                                   \
        uint32_t bb_i1 = bb_insns(), bb_c1 = bb_cycles();       \
        bb_add((series), bb_c1 - bb_c0, bb_i1 - bb_i0);         \
    } while (0)

/* WARMUP untimed, then SAMPLES timed runs of STMT into a reset series. */
#define BB_RUN(series, STMT)                                    \
    do {                                                        \
        for (uint32_t bb_w = 0; bb_w < BB_WARMUP; bb_w++) {     \
            STMT;                                               \
        }                                                       \
        bb_reset(series);                                       \
        for (uint32_t bb_k = 0; bb_k < BB_SAMPLES; bb_k++)      \
            BB_TIME((series), STMT);                            \
    } while (0)

/* Measured clock: cycles over a tenth of a second of the system counter. */
static inline uint64_t bb_cpu_hz(void)
{
    uint32_t frq = bb_cntfrq();
    uint64_t v0 = bb_cntvct(), v1;
    uint32_t c0 = bb_cycles(), c1;
    do {
        v1 = bb_cntvct();
    } while (v1 - v0 < frq / 10u);
    c1 = bb_cycles();
    return (uint64_t)(c1 - c0) * frq / (v1 - v0);
}

static void bb_sort(uint32_t *a, uint32_t n)
{
    /* Heap sort: no recursion, no allocation, n log n. */
    for (uint32_t start = n / 2; start-- > 0;) {
        for (uint32_t root = start;;) {
            uint32_t child = 2 * root + 1;
            if (child >= n) break;
            if (child + 1 < n && a[child] < a[child + 1]) child++;
            if (a[root] >= a[child]) break;
            uint32_t t = a[root]; a[root] = a[child]; a[child] = t;
            root = child;
        }
    }
    for (uint32_t end = n; end-- > 1;) {
        uint32_t t = a[0]; a[0] = a[end]; a[end] = t;
        for (uint32_t root = 0;;) {
            uint32_t child = 2 * root + 1;
            if (child >= end) break;
            if (child + 1 < end && a[child] < a[child + 1]) child++;
            if (a[root] >= a[child]) break;
            t = a[root]; a[root] = a[child]; a[child] = t;
            root = child;
        }
    }
}

/* sorted[n * per_mille / 1000], ipc-bench's index; per mille 1000 is the
 * largest. */
static inline uint32_t bb_at(const uint32_t *sorted, uint32_t n, uint32_t per_mille)
{
    uint64_t i = (uint64_t)n * per_mille / 1000u;
    if (n == 0)
        return 0;
    return sorted[i < n ? i : n - 1];
}

static inline unsigned long long bb_ns(uint64_t cycles, uint64_t hz)
{
    return (unsigned long long)(cycles * 1000000000ull / hz);
}

static void bb_print_clock(const char *prog, uint64_t hz)
{
    BB_PRINTF("%s clock cpu_hz=%llu cntfrq=%lu\n", prog, (unsigned long long)hz,
              (unsigned long)bb_cntfrq());
}

/* Print a series' lines (and sort it). */
static void bb_print(const char *prog, const char *what, struct bb_series *s, uint64_t hz)
{
    uint32_t n = s->n;
    if (n == 0) {
        BB_PRINTF("%s %s n=0\n", prog, what);
        return;
    }
    bb_sort(s->cyc, n);
    bb_sort(s->ins, n);
    BB_PRINTF("%s %s n=%lu min=%llu p50=%llu p90=%llu p99=%llu mean=%llu\n", prog, what,
              (unsigned long)n, bb_ns(bb_at(s->cyc, n, 0), hz), bb_ns(bb_at(s->cyc, n, 500), hz),
              bb_ns(bb_at(s->cyc, n, 900), hz), bb_ns(bb_at(s->cyc, n, 990), hz),
              bb_ns(s->cyc_sum / n, hz));
    BB_PRINTF("%s %s.cycles n=%lu min=%lu p50=%lu p90=%lu p99=%lu mean=%llu\n", prog, what,
              (unsigned long)n, (unsigned long)bb_at(s->cyc, n, 0),
              (unsigned long)bb_at(s->cyc, n, 500), (unsigned long)bb_at(s->cyc, n, 900),
              (unsigned long)bb_at(s->cyc, n, 990), (unsigned long long)(s->cyc_sum / n));
    BB_PRINTF("%s %s.ins n=%lu min=%lu p50=%lu p90=%lu p99=%lu mean=%llu\n", prog, what,
              (unsigned long)n, (unsigned long)bb_at(s->ins, n, 0),
              (unsigned long)bb_at(s->ins, n, 500), (unsigned long)bb_at(s->ins, n, 900),
              (unsigned long)bb_at(s->ins, n, 990), (unsigned long long)(s->ins_sum / n));
    BB_PRINTF("%s %s.cycles.pct v=", prog, what);
    for (uint32_t p = 0; p <= 100; p++)
        BB_PRINTF(p ? ",%lu" : "%lu", (unsigned long)bb_at(s->cyc, n, p * 10));
    BB_PRINTF("\n");
}

#endif
