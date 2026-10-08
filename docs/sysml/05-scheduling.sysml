/*
 * Ferrix — SysML v2 model, part 5: processors, time, and scheduling.
 *
 * Stages 3 to 5 run today: interrupts, a clock, every processor online,
 * IPIs, TLB shootdown, grace periods, fair locks, and tasks scheduled by
 * EEVDF in one Throughput domain. Stage 14's real-time domains are designed
 * here (docs/ARCHITECTURE.md §5) and not yet written.
 */
package FerrixScheduling {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;

    // -----------------------------------------------------------------------
    // Synchronisation (src/lib/kernel/sync)
    // -----------------------------------------------------------------------

    #implemented part def TicketSpinLock {
        @stage { number = 4; }
        doc /* Fair by construction: arrival order is acquisition order, so the
             * worst-case wait is bounded by the CPUs ahead rather than by
             * luck. An unfair lock on a starved core is a stage-14 latency
             * bug nobody will find. Rules the callers keep: never a plain
             * SpinLock from an interrupt handler, never twice on one CPU, one
             * global order, never sleep inside. lock_manually / force_unlock
             * exist for the one case a guard cannot express: a run queue lock
             * handed from the outgoing context to the incoming one across a
             * context switch. */
        action lock;
        action lockManually;
        action forceUnlock;
    }
    #implemented part def IrqSpinLock :> TicketSpinLock {
        doc /* The same, with interrupts masked for the duration through an
             * IrqControl implemented over each architecture's mask. */
    }
    #implemented part def Once;
    #writtenAhead part def RwSpinLock {
        doc /* Many readers or one writer, writer-preferring. Not yet reached. */
    }

    // -----------------------------------------------------------------------
    // Interrupts and time (src/kernel/src/irq.rs, src/kernel/src/timer.rs)
    // -----------------------------------------------------------------------

    #implemented part def IrqTable {
        @stage { number = 3; }
        doc /* A table of 1024 slots behind an interrupt-masking lock; dispatch
             * copies the handler out and runs it after release, so two CPUs
             * taking interrupts contend for a load, not for each other's
             * handlers. The acknowledge protocol stays in arch; what arrives
             * here is the number. Nothing is ever unregistered: stage 10's
             * Interrupt objects register one kernel handler per line for
             * good and find the line's holder in a table of their own
             * (src/kernel/src/object/interrupt.rs), so the grace period a
             * removal would need is never paid. */
        attribute slots : Natural = 1024;
        attribute delivered : Natural;
        attribute unclaimed : Natural;
        action register;
        action dispatch;
    }

    #implemented part def Timer {
        @stage { number = 3; }
        doc /* Two things deliberately kept apart. The counter answers "how
             * long since boot" and is read, never waited on. The timer is an
             * interrupt scheduled for a future instant. One-shot is the
             * primitive, because the Arm timers compare against an absolute
             * instant and a tickless scheduler wants one-shot anyway. */
        action afterNanos { doc /* One-shot, in nanoseconds. */ }
        action every {
            doc /* Periodic as a schedule: tick n is due at start + n * interval
                 * and the instant tick n-1 arrived has no say in it, so a late
                 * tick is absorbed rather than propagated. Past sixteen
                 * intervals behind, the debt is written off. */
        }
        action stop;
        action ticks;
        action nowNanos;
        action counterHz;
        attribute catchUpLimit : Natural = 16;
    }

    // -----------------------------------------------------------------------
    // Processors (src/kernel/src/smp.rs)
    // -----------------------------------------------------------------------

    #implemented part def PerCpu {
        @stage { number = 4; }
        doc /* One record per processor, allocated once and never freed, with
             * a register pointing at it: GS base, TPIDR_EL1, or TPIDRPRW. It
             * holds its own address first so gs:0 is a pointer. Every
             * processor checks its record against what its hardware says it
             * is. */
        attribute logicalIndex : Natural;
        attribute hardwareId : Natural;
        attribute online : Boolean;
        attribute ipisTaken : Natural;
        ref part stack : KernelStack;
        #implemented part runqueue : Runqueue { @stage { number = 5; } }
        attribute needResched : Boolean;
    }

    part def KernelStack;

    #implemented part def Smp {
        @stage { number = 4; }
        doc /* Discover, start, and coordinate every processor. */
        part topology : PerCpu[1..*] ordered;
        action discover {
            doc /* From the MADT or the device tree; duplicates refused; the
                 * reading processor must be among them. */
        }
        action startSecondaries;
        action secondaryMain {
            doc /* Bring up this core's interrupt controller interface, mark
                 * online, then sleep in sti;hlt or wfi between pieces of work,
                 * woken by a broadcast IPI. */
        }
        action runEverywhere;
        action flushTlbEverywhere {
            doc /* A shootdown IPI where the hardware does not broadcast
                 * invalidation (x86-64, which invalidates global entries by
                 * toggling CR4.PGE); nothing extra where it does. */
        }
        action readSection {
            doc /* A read-side critical section: interrupts masked. */
        }
        action synchronize {
            doc /* The grace period: interrupt every other processor and wait
                 * for each to take it, which none can inside a section. What
                 * stage 14's mode switch will wait on. Stage 10 removes no
                 * interrupt handler after all, so only stage 4's boot check
                 * calls it today. */
        }
        attribute shootdowns : Natural;
        attribute gracePeriods : Natural;

        attribute offlining : Boolean {
            @deferred { reason = "Nothing takes a processor offline; records and stacks live for the life of the machine."; }
        }
    }

    // -----------------------------------------------------------------------
    // Tasks and the scheduler, stage 5
    // -----------------------------------------------------------------------

    enum def TaskState { Runnable; Blocked; Dead; }

    enum def SchedClass {
        doc /* Highest first. src/lib/kernel/sched names Fair and Idle today; the
             * real-time classes are stage 14's. */
        Edf;
        Fifo;
        RoundRobin;
        Eevdf;
        Idle;
    }

    enum def LinuxPolicy { SchedOther; SchedBatch; SchedIdle; SchedFifo; SchedRr; SchedDeadline; }

    #implemented part def Task {
        @stage { number = 5; }
        doc /* src/kernel/src/sched/task.rs: a kernel thread — a guard-paged stack,
             * a saved stack pointer, and the bookkeeping that says where it
             * is. Everything about choosing between tasks is src/lib/kernel/sched's.
             * The saved stack pointer is touched only by the CPU that holds
             * the owning run queue's lock at that moment. From stage 6 also a
             * user thread, 1:1, forced by the ABI; one of the nine kernel
             * objects. */
        attribute taskState : TaskState;
        attribute weight : Natural { doc /* From nice: nice 0 is 1024. */ }
        attribute affinity : Natural {
            doc /* The processors it may run on, one bit each. Replaced a
                 * `pinned` boolean: a task pinned to one processor is an
                 * affinity of one, which is the same thing said once rather
                 * than twice. */
        }
        attribute runtimeNanos : Natural;
        attribute switches : Natural;
        attribute sleepDeadline : Natural[0..1];
        part stack : KernelStack;
        ref part cpu : PerCpu;
        ref part addressSpace : FerrixMemory::ProcessAddressSpace[0..1] {
            @stage { number = 6; }
            doc /* The address space its user half is translated through, absent
                 * for a kernel thread. A reference and not a part: the space
                 * outlives no task in particular and two threads of one process
                 * share it. Holding it is what keeps the tables alive while a
                 * processor walks them -- the root register is installed on the
                 * way into the task, and this reference is the only thing
                 * between those tables and the frame allocator. */
        }
        ref part thread : UserThread[0..1] {
            @stage { number = 6; }
            doc /* The user thread it runs, absent for a kernel thread. Holding
                 * it is what keeps the thread, and through it the process,
                 * alive while the task is. */
        }
        #planned attribute schedClass : SchedClass { @stage { number = 14; } }
        #planned attribute policy : LinuxPolicy { @stage { number = 7; } }
        #planned attribute priority : Natural { @stage { number = 14; } doc /* 1 to 99 for FIFO/RR. */ }
        #planned attribute bandwidth : Natural { @stage { number = 14; } doc /* CBS reservation for EDF. */ }
    }

    #implemented part def UserThread {
        @stage { number = 6; }
        doc /* A line of execution through a program, as the scheduler sees
             * it: src/kernel/src/sched/task.rs's UserThread trait. The thread
             * itself is the personality's -- its id, its signals, the address
             * to clear when it ends are none of the scheduler's business --
             * so the scheduler asks it one thing, the process it runs in, to
             * count the thread starting and gone. */
        ref part process : FerrixObjects::Process;
    }

    #implemented part def Tasks {
        @stage { number = 5; }
        doc /* src/kernel/src/sched/mod.rs: spawn, spawn_on, exit, yield, sleep,
             * wake, reap. Preemption happens only on the way out of an
             * interrupt: the timer sets need_resched and returns, and the trap
             * path decides once the controller has been told the interrupt is
             * done, because switching inside the handler would leave it in
             * service for as long as the next task ran. */
        part tasks : Task[0..*];
        action spawnKernelThread;
        action spawnInAddressSpace {
            @stage { number = 6; }
            doc /* Start a task that has an address space. The entry point is
                 * still a kernel function: having a space and running in it at
                 * a lower privilege level are separate steps, and a processor
                 * must be able to do the first without the second, because
                 * kernel code servicing a fault runs in the space that
                 * faulted. */
        }
        action switchTo { doc /* Deciding and switching are one operation under the queue lock. */ }
        action swapAddressSpace {
            @stage { number = 6; }
            doc /* Install the incoming task's root, inside choose_next, under
                 * the run queue lock and before the registers move -- not in
                 * the architecture's switch, which takes two stack pointers and
                 * does register operations, and not after, where the incoming
                 * context has already resumed. Compared by pointer because two
                 * threads of one process share a root and, on x86-64 and
                 * AArch64, installing one invalidates every user translation
                 * the processor had (ARMv7-A's ASIDs keep them, but its install
                 * still writes TTBR0): switching within a process must cost
                 * nothing. A kernel thread has the user half switched off
                 * rather than left installed, which is the plain alternative to
                 * lazy TLB and does not oblige anyone to keep a space alive
                 * under a thread holding no reference to it. */
        }
        action placeTask {
            doc /* Chosen on spawn rather than inherited from the creator. Not
                 * chosen again on wake: that is what Linux does and is better,
                 * but a blocked task is not an unattached one — it may be on a
                 * wait queue, in a sleeper set, part-way into blocking and in
                 * neither, or in both — and moving it from under one of those
                 * wedged the machine. Naming those states and giving them an
                 * order is its own change; the balancer covers the same ground
                 * less promptly meanwhile. */
        }
        action exitTask;
        action yieldNow;
        action sleepUntil;
        action wake { doc /* Places the task and wakes an idle processor, which is otherwise never told. */ }
        action preemptOnIrqExit;
        action reap;
        part waitQueues : WaitQueue[0..*];
    }

    #implemented part def WaitQueue {
        @stage { number = 5; }
        doc /* src/kernel/src/sched/wait.rs. The lost wake-up between a waiter and
             * a waker is closed by order: a waiter marks itself blocked and
             * joins the queue before its last look at the condition; a waker
             * takes the same lock, so it either sees the waiter or made the
             * condition true before that look.
             *
             * That argument is necessary and was not sufficient, and the gap
             * is worth recording because it cost a day. It assumes a waker
             * exists. Seven waits watched a counter that was incremented when
             * a task *started* and signalled by nothing, because the only
             * wake came from a task *finishing* — so they slept their entire
             * deadline, woke on the timer, found the condition true and
             * reported success. Twenty seconds each, silently, indistinguishable
             * from a slow machine.
             *
             * Two rules follow. Every store to a counter a predicate reads is
             * followed by a notify. And a wait sleeps in slices rather than in
             * one span to its deadline, so the next missing notify costs
             * milliseconds instead of the whole budget. */
        action waitUntilDeadline {
            doc /* Bounded, and off the queue however it leaves: a waiter that
                 * returns while still listed is woken by the next wakeAll, out
                 * of whatever it is doing by then. */
        }
        action notify { doc /* Called wherever a watched counter is stored. */ }
        action wakeAll;
    }

    #implemented part def Runqueue {
        @stage { number = 5; }
        doc /* src/kernel/src/sched/queue.rs: one per CPU, one plain SpinLock taken
             * with interrupts masked and handed across the switch. Tickless:
             * the timer is armed for the end of the running task's slice or
             * the first sleeper's wake-up, whichever is first, and not at all
             * for one task with nothing behind it. Work stealing by an idle
             * processor in Throughput; none in HardRt, because partitioned
             * scheduling is what makes the admission test valid. A processor
             * that is idle too keeps its last waiting task: it has been handed
             * it and kicked, and taking it leaves it to halt again with
             * nothing to wake it while the stealer gathers a whole burst. */
        attribute targetLatencyNanos : Natural = 3000000 {
            doc /* Shared among whatever is runnable rather than handed to each
                 * in full. A fixed slice is also a per-task latency bound: the
                 * last of n runnable tasks waits n slices, which at a thousand
                 * threads and three milliseconds each is three seconds before
                 * the last one is looked at. */
        }
        attribute minSliceNanos : Natural = 375000 {
            doc /* The floor. Taken to its conclusion the arithmetic asks for
                 * slices of nanoseconds and the machine does nothing but
                 * switch; below this the period stretches instead. The ratio
                 * to the target decides how many runnable tasks it takes
                 * before latency has to give, which here is eight. */
        }
        part load : LoadAverage;
        part fair : EevdfRunQueue;
        ref part current : Task[0..1];
        ref part idle : Task;
        ref part sleepers : Task[0..*] ordered;
        action pickNext;
        action account;
        action armTimer;
        action stealCandidate;
        action shouldPreempt;
        action checkInvariants;
    }

    #implemented part def EevdfRunQueue {
        @stage { number = 5; }
        doc /* src/lib/kernel/sched: entities with weight and virtual runtime; the
             * queue's virtual time is the weight-average; an entity is
             * eligible when its virtual runtime is at or behind it; the pick
             * is the eligible entity with the earliest virtual deadline. The
             * tree is an AVL ordered by deadline, each subtree remembering its
             * minimum virtual runtime, so the pick is one O(log n) walk. Lag
             * stays within one request, which is what the boot test measures. */
        attribute nice0Weight : Natural = 1024;
        action insert;
        action remove;
        action pick;
        action release { doc /* For a steal: hand the entity's state to another queue. */ }
    }

    attribute def CpuLoad {
        doc /* One processor as a placement or balancing decision sees it. */
        attribute queued : Natural { doc /* Entities on its queue, running one included. */ }
        attribute average : Natural { doc /* Its decaying load, in units of a nice-0 task. */ }
        attribute idle : Boolean { doc /* Nothing runnable, rather than "running the idle task": a processor just given a task still has idle current until it next schedules, and a burst would otherwise all pile on behind the first. */ }
    }

    #implemented part def LoadAverage {
        @stage { number = 5; }
        doc /* src/lib/kernel/sched/balance.rs. A geometric decay with a 33-millisecond
             * half-life, in the shape of Linux's PELT, measuring *weighted
             * demand* rather than occupancy — a processor is either running
             * something or it is not, so "busy" saturates at one task and says
             * nothing after that, which leaves a balancer nothing to compare.
             * Four runnable nice-0 tasks read four times one.
             *
             * Carried with ten bits of extra precision, which is not a detail:
             * each step truncates twice, and without them the loss balances the
             * gain at about 978 of 1024, so a permanently busy processor would
             * report 95% forever and every comparison would be against a
             * ceiling nothing could reach. */
        attribute scale : Natural = 1024;
        attribute periodNanos : Natural = 1048576;
        attribute halfLifePeriods : Natural = 32;
        action accumulate { doc /* A level held for an interval, so a caller sampling on ticks and one sampling on switches describe the same history. */ }
        action decay;
    }

    #implemented part def Placement {
        @stage { number = 5; }
        doc /* Where a task should run, asked on spawn and folded one processor
             * at a time rather than snapshotted into an array — the array was
             * sized for 256 processors, six kilobytes of a sixteen-kilobyte
             * kernel stack, and the balancing caller asks from inside an
             * interrupt on the stack of whatever it interrupted.
             *
             * The order: the preferred processor if it is idle, since nothing
             * beats staying where the caches are; then any idle processor,
             * because idle capacity is waste and this is the case a burst of
             * spawns otherwise queues behind itself; then fewest queued, and
             * only then least loaded.
             *
             * Fewest-queued before least-loaded is not a refinement, it is the
             * fix to a real defect. The count moves the instant a task is
             * placed and is the only thing here that shows a placer the effect
             * of its own last decision; the average is a decaying history that
             * cannot move inside a burst. Ranking on the average first sends
             * every task in a burst to whichever processor has been idle
             * longest. Found on an STM32MP157D-DK1, where a two-task burst on
             * a quiet machine put both on one core. */
        action consider;
        action choice;
    }

    #implemented part def Balancing {
        @stage { number = 5; }
        doc /* Moving work that is already placed. Stealing by an idle
             * processor covers the case that matters most and costs nothing,
             * because a processor about to idle is not busy. This covers the
             * other: every processor busy, one much busier.
             *
             * It pushes as well as pulls, and on a tickless kernel the push is
             * the one that works. A processor alone with one task is never
             * interrupted — arming a timer would buy nothing, which is where
             * tickless comes from — so an under-loaded processor never reaches
             * the balancer to pull anything towards itself. The overloaded one
             * is interrupted constantly, precisely because it has tasks to
             * switch between, so it is the only one awake to notice.
             *
             * Two tests, not one, and the second is a brake. The load average
             * is deliberately slow, so moving a task does not change it for
             * tens of milliseconds and a balancer consulting only the average
             * keeps moving more: eight movable tasks were observed moving over
             * a thousand times. The queue count updates instantly, so a
             * difference of at least two is required as well. */
        attribute thresholdFraction : Natural = 2 { doc /* Half a nice-0 task, because the imbalance moved is half the difference. */ }
        attribute intervalNanos : Natural = 16000000;
        action pullFrom;
        action pushTo;
    }

    enum def DomainMode {
        Throughput;
        SoftRt;
        HardRt;
    }

    attribute def ModeSpec {
        attribute mode : DomainMode;
        attribute classes : SchedClass[1..*] ordered;
        attribute preemption : String;
        attribute interrupts : String;
    }

    #implemented part def SchedulingDomain {
        @stage { number = 5; }
        doc /* src/lib/kernel/sched/domain.rs. CPUs are partitioned into domains, each
             * in one of three modes, changeable at runtime. Per domain, not
             * global: a four-core machine runs a HardRt partition on one core
             * and Throughput on the other three, with rustc on the latter.
             * Stage 5 builds one domain holding every CPU (up to 256) in
             * Throughput; the mode is a property of the domain from the first
             * line, the class stack is looked up from the mode, and SoftRt
             * and HardRt are named and refused until stage 14. check_partition
             * requires the domains to cover the online CPUs exactly once. */
        ref part cpus : PerCpu[1..*];
        attribute mode : DomainMode = DomainMode::Throughput;
        attribute classStack : SchedClass[1..*] ordered;
        exhibit state lifecycle : DomainLifecycle;

        attribute throughput : ModeSpec = ModeSpec(mode = DomainMode::Throughput, classes = (SchedClass::Eevdf, SchedClass::Idle), preemption = "preemption points, deferred for cache locality", interrupts = "in context");
        attribute softRt : ModeSpec = ModeSpec(mode = DomainMode::SoftRt, classes = (SchedClass::Fifo, SchedClass::RoundRobin, SchedClass::Eevdf, SchedClass::Idle), preemption = "fully preemptible kernel, PI mutexes", interrupts = "threaded");
        attribute hardRt : ModeSpec = ModeSpec(mode = DomainMode::HardRt, classes = (SchedClass::Edf), preemption = "fully preemptible, bounded sections", interrupts = "threaded, bandwidth-reserved");
    }

    attribute def ModeSwitchRequest {
        attribute target : DomainMode;
    }
    attribute def GracePeriodElapsed;

    #planned state def DomainLifecycle {
        @stage { number = 14; }
        doc /* Switching modes takes the domain through a quiescent point.
             * Tasks the new mode cannot represent are demoted with an errno
             * the switching caller sees, never silently. */
        entry; then running;
        state running;
        transition running_to_draining
            first running
            accept request : ModeSwitchRequest
            then draining;
        state draining {
            doc /* Stop admitting. */
        }
        transition draining_to_migrating
            first draining
            accept elapsed : GracePeriodElapsed
            then migrating;
        state migrating {
            do action migrateTasks {
                doc /* Move tasks whose policy the new mode does not offer;
                     * demote with errno what cannot move. */
            }
        }
        transition migrating_to_swapping first migrating then swapping;
        state swapping {
            do action swapClassStack;
        }
        transition swapping_to_running first swapping then running;
    }

    #implemented part def Scheduler {
        @stage { number = 5; }
        doc /* Not one policy: a class stack per domain. EEVDF rather than CFS
             * for the fair class, because each task gets an actual eligible
             * time and a deadline, so latency has a bound and not only
             * fairness. Tickless: the timer is armed for the next decision. */
        part domains : SchedulingDomain[1..*];
        part fair : EevdfClass;
        part idle : IdleClass;
        #implemented part loadBalancing : LoadBalancing {
            doc /* Stage 5 left nothing beyond work stealing by an idle
                 * processor; Placement and Balancing above were added after
                 * it: src/kernel/src/sched/mod.rs's balance(), at most every 16
                 * milliseconds per processor, pulling and pushing over
                 * src/lib/kernel/sched's arithmetic. */
        }
        #planned part fifoRr : FifoRrClass { @stage { number = 14; } }
        #planned part edf : EdfClass { @stage { number = 14; } }
        action pickNext;
        action tick;
        #inProgress action setScheduler {
            @stage { number = 7; }
            doc /* Linux sched_setscheduler: SCHED_FIFO/RR to the soft-RT
                 * classes, SCHED_DEADLINE to EDF, SCHED_OTHER/BATCH/IDLE to
                 * fair. src/kernel/src/syscall/limits.rs answers it today with
                 * the one policy there is a class for: SCHED_OTHER at
                 * priority zero is accepted and every other policy is EINVAL,
                 * refused rather than accepted and ignored, until stage 14
                 * brings the classes. The nice value is the fair class's
                 * weight already. */
        }
        #planned action switchDomainMode { @stage { number = 14; } }
    }

    part def LoadBalancing;

    part def EevdfClass {
        doc /* Eligible virtual deadline first. Weight and bandwidth arrive
             * from the cpu cgroup controller at stage 13. */
    }
    part def IdleClass;
    part def FifoRrClass {
        doc /* Priorities 1 to 99, with priority-inheritance mutexes and
             * threaded interrupts in SoftRt. */
    }
    part def EdfClass {
        doc /* Earliest deadline first with constant-bandwidth-server
             * admission control that refuses an unschedulable set instead of
             * missing deadlines. Preallocated pools on the RT path; no
             * dynamic allocation there. */
        action admit;
    }

    #implemented part def Futex {
        @stage { number = 7; }
        doc /* src/kernel/src/syscall/futex.rs: wait, wake and requeue, plain and
             * with a bitset, with the word read and the waiter listed under
             * one table lock so no wake is lost, and the read under it
             * never faulting. One table. A private futex is keyed by
             * address space and user address; one without
             * FUTEX_PRIVATE_FLAG on a word in a shared region, by the VMO
             * behind it and the word's offset there, so a word two
             * processes map shared is one futex, as on Linux. A thread
             * that ends clears its clear_child_tid word and wakes whoever
             * waits on it. The robust list's head is recorded and never walked, and
             * the priority-inheritance operations and FUTEX_WAKE_OP are
             * ENOSYS. */
    }
}
