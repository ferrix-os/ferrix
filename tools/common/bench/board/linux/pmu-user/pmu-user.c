// SPDX-License-Identifier: GPL-2.0
/*
 * pmu-user: give user mode the ARMv7 PMU for the board bench
 * (docs/BOARD-BENCH.md; the timing contract is
 * tools/common/bench/board/common/board-bench.h).
 *
 * On every online CPU it sets PMUSERENR.EN (bit 0), so PL0 may read and
 * program the cycle counter and the event counters, and prints SCTLR, ACTLR
 * and CNTKCTL once, so the boot log shows the caches, the coherency and the
 * counter access each CPU runs with. Unloading restores each CPU's
 * PMUSERENR.
 *
 * Measurement only: with EN set any process may program the counters. A CPU
 * brought online after loading is not covered.
 */
#include <linux/init.h>
#include <linux/module.h>
#include <linux/percpu.h>
#include <linux/printk.h>
#include <linux/smp.h>
#include <asm/barrier.h>

#define PMUSERENR_EN 0x1u
#define CNTKCTL_PL0VCTEN 0x2u

static DEFINE_PER_CPU(u32, saved_pmuserenr);
static DEFINE_PER_CPU(bool, enabled);

#define CP15_READ(name, op1, crn, crm, op2)                                     \
	static inline u32 read_##name(void)                                     \
	{                                                                       \
		u32 v;                                                          \
		asm volatile("mrc p15, " #op1 ", %0, " #crn ", " #crm ", " #op2 \
			     : "=r"(v));                                        \
		return v;                                                       \
	}

CP15_READ(midr, 0, c0, c0, 0)
CP15_READ(id_pfr1, 0, c0, c1, 1)
CP15_READ(id_dfr0, 0, c0, c1, 2)
CP15_READ(sctlr, 0, c1, c0, 0)
CP15_READ(actlr, 0, c1, c0, 1)
CP15_READ(pmuserenr, 0, c9, c14, 0)
CP15_READ(cntkctl, 0, c14, c1, 0)

static inline void write_pmuserenr(u32 v)
{
	asm volatile("mcr p15, 0, %0, c9, c14, 0" : : "r"(v));
	isb();
}

/* ID_DFR0.PerfMon: 0 is none, 0xf is no architected PMU. */
static bool has_pmu(void)
{
	u32 perfmon = (read_id_dfr0() >> 24) & 0xf;

	return perfmon != 0 && perfmon != 0xf;
}

/* ID_PFR1.GenTimer: CNTKCTL exists only with the generic timer. */
static bool has_generic_timer(void)
{
	return ((read_id_pfr1() >> 16) & 0xf) != 0;
}

static void pmu_user_enable(void *unused)
{
	u32 old = read_pmuserenr();
	u32 cntkctl = has_generic_timer() ? read_cntkctl() : 0;

	this_cpu_write(saved_pmuserenr, old);
	write_pmuserenr(old | PMUSERENR_EN);
	this_cpu_write(enabled, true);
	pr_info("pmu-user: cpu%d MIDR=%08x SCTLR=%08x ACTLR=%08x CNTKCTL=%08x (PL0VCTEN=%u) PMUSERENR=%08x->%08x ID_DFR0=%08x\n",
		smp_processor_id(), read_midr(), read_sctlr(), read_actlr(),
		cntkctl, !!(cntkctl & CNTKCTL_PL0VCTEN), old, read_pmuserenr(),
		read_id_dfr0());
}

static void pmu_user_restore(void *unused)
{
	if (this_cpu_read(enabled)) {
		write_pmuserenr(this_cpu_read(saved_pmuserenr));
		this_cpu_write(enabled, false);
	}
}

static int __init pmu_user_init(void)
{
	if (!has_pmu()) {
		pr_err("pmu-user: ID_DFR0=%08x: no architected PMU\n",
		       read_id_dfr0());
		return -ENODEV;
	}
	on_each_cpu(pmu_user_enable, NULL, 1);
	return 0;
}

static void __exit pmu_user_exit(void)
{
	on_each_cpu(pmu_user_restore, NULL, 1);
	pr_info("pmu-user: PMUSERENR restored\n");
}

module_init(pmu_user_init);
module_exit(pmu_user_exit);
MODULE_DESCRIPTION("User-mode PMU access for the board bench (measurement only)");
MODULE_LICENSE("GPL");
