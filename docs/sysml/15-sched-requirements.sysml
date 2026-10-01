/*
 * Ferrix — SysML v2 model, part 15: the low-level requirements of sched/.
 *
 * What each unit of src/kernel/src/sched/ does, as `ItemLowLevel` requirements
 * (part 13 defines the format, part 14's pilot settled how they are cut).
 * The scheduling decision itself is `src/lib/kernel/sched`'s `RunQueue`, an
 * EEVDF queue the kernel's per-processor queue wraps; its host tests are the
 * checks, and each requirement names the kernel function that carries the
 * behaviour into the item as its `unit`.
 *
 * The slice starts with the two requirements FX-0502's fix created. The
 * rest of sched/ is check-writing work, in the baseline until then.
 *
 * Ids are `L.sched.<n>`, numbered through the subsystem.
 */
package FerrixSchedRequirements {
    private import FerrixItemRequirements::*;

    package Decisions {
        doc /* src/kernel/src/sched/queue.rs and src/kernel/src/sched/mod.rs: when a
             * processor next decides, and what a yield asks for. */

        requirement <'L.sched.1'> aDecisionWithinASlice : ItemLowLevel {
            attribute :>> statement = "While any task waits on a processor's
                queue, CpuQueue::arm_timer shall arm the next decision no more
                than one configured slice away, however long a request the
                running task holds.";
            attribute :>> criterion = "With an entity queued that is not
                eligible and the running one having yielded 600 times, so that
                its remaining request exceeds 100 slices, the queue's next
                decision is at most one slice away; with nothing queued there
                is none.";
            attribute :>> parent = "H.SCHED.2";
            attribute :>> unit = "sched::queue::CpuQueue::arm_timer";
        }
        requirement <'L.sched.2'> aLoneYieldAsksNothing : ItemLowLevel {
            attribute :>> statement = "A yield by the only task on a processor's
                queue shall leave its request unchanged.";
            attribute :>> criterion = "After 600 yields with nothing else
                queued, the running entity's remaining slice is what it was
                before the first.";
            attribute :>> parent = "H.SCHED.2";
            attribute :>> unit = "sched::yield_now";
        }
        requirement <'L.sched.3'> aThrottledTaskLeavesForAKill : ItemLowLevel {
            attribute :>> statement = "throttle_current shall hold a task whose job
                or one above it has used its cpu.max quota until the period ends,
                looking every millisecond whether the way back to user mode has a
                kill, a signal or a stop to deal with, and leaving the wait if so.";
            attribute :>> criterion = "A program in a job with cpu.max 1000 1000000
                that has used its millisecond ends within a quarter of a second of
                its SIGKILL, where waiting out the period takes nine tenths of a
                second.";
            attribute :>> parent = "H.QUOTA.10";
            attribute :>> unit = ("sched::throttle_current", "trap::must_attend");
        }
        requirement <'L.sched.4'> aQuotaThatComesLateBinds : ItemLowLevel {
            attribute :>> statement = "A cpu.max written for a job, or a task moved
                beneath one, shall arm the timer of every processor running a
                task of the job, so that a task alone on its processor, which
                gets no tick, is cut when its quota is used up.";
            attribute :>> criterion = "A program already running alone on its
                processor is held to between a twelfth and two thirds of a
                processor by a cpu.max 20000 100000 written under it, and by being
                moved beneath one.";
            attribute :>> parent = "H.QUOTA.10";
            attribute :>> unit = ("sched::rearm_timers", "sched::rearm_here",
                "sched::set_task_group", "sched::note_moved");
        }
    }
}
