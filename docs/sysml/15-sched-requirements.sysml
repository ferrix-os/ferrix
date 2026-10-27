/*
 * Ferrix — SysML v2 model, part 15: the low-level requirements of sched/.
 *
 * What each unit of src/kernel/src/sched/ does, as `ItemLowLevel` requirements
 * (part 13 defines the format, part 14's pilot settled how they are cut).
 * The scheduling decision itself is `src/lib/kernel/sched-fair`'s `RunQueue`, an
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
                and leave the wait before then when a kill, a signal or a stop is
                posted to the task.";
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

    package Wakes {
        doc /* src/kernel/src/timer.rs and src/kernel/src/sched/mod.rs: the
             * timer's skipped arm, the clock, and the decisions a channel
             * round trip's wakes defer (docs/OPAQUE-KERNEL.md §9.4, items 1,
             * 2, 3 and 6; the consultant's conditions of 2026-10-02). */

        requirement <'L.sched.5'> aSkippedArmIsNeverLate : ItemLowLevel {
            attribute :>> statement = "timer::after and timer::after_from shall
                keep, for each processor, the clock read after the hardware was
                armed plus the delay asked for, and the deadline that arm was
                asked for; they shall leave a one-shot armed only when either is
                no later than the new deadline, and otherwise arm for the new one,
                so that no interrupt asked for is served later than the hardware's
                own one-tick rounding of a delay shorter than a tick plus the time
                from the clock reading the armed one-shot's deadline was taken from
                to the end of its write. timer::stop shall disarm a
                periodic timer and leave a one-shot to fire.";
            attribute :>> criterion = "On the boot processor, a 2 ms one-shot asked
                for while a 1 s one is armed, and one asked for before a 1 s one,
                each fire within 200 ms of the 2 ms deadline (the `arm` line); and
                one 2 ms deadline asked for twice from one clock reading, fired and
                then stopped, writes the timer once.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("timer::after", "timer::after_from", "timer::stop",
                "timer::on_tick", "timer::armed_slot");
        }
        requirement <'L.sched.6'> theClockIsTheWideFormula : ItemLowLevel {
            attribute :>> statement = "now_nanos shall convert the counter to
                nanoseconds as ticks times 10^9 over the counter's rate, rounded
                down, exactly, and u64::MAX past what 64 bits hold.";
            attribute :>> criterion = "At 12 counter rates from 1 Hz to u64::MAX
                and 10 tick values at each, from 0 to u64::MAX across a second's
                boundaries and the saturation point, the two-division conversion
                equals the 128-bit formula (ferrix-vdso's host test).";
            attribute :>> parent = "H.SCHED.9";
            attribute :>> unit = ("timer::now_nanos", "timer::ticks_to_nanos");
        }
        requirement <'L.sched.7'> aDeferredDecisionIsBounded : ItemLowLevel {
            attribute :>> statement = "A wake made inside a system call shall
                leave its decision to the call's end (sched::call_left), the next
                preempt_enable that brings the count to zero, or the next interrupt
                exit, whichever comes first; and a Sync wake shall arm the waker's
                processor's timer for its next decision, so that a waker that does
                not block leaves the woken task waiting at most one slice.";
            attribute :>> criterion = "Argued in MEMORY-AND-TIMING §2.2c: no check
                times the deferral, which sits inside H.SCHED.2's bound of one slice
                plus the timer overruns served.";
            attribute :>> parent = "H.SCHED.2";
            attribute :>> unit = ("sched::resched_here", "sched::call_entered",
                "sched::call_left", "sched::carry_in_call", "sched::kick_after_wake",
                "sched::wake_at_home");
        }
        requirement <'L.sched.8'> aSyncWakeMovesOnlyWhereAllowed : ItemLowLevel {
            attribute :>> statement = "A Sync wake shall move the task it wakes
                onto the waker's processor only when the task is asleep at its
                home and in nothing else, its affinity allows the waker's
                processor, and nothing else is queued there, deciding under both
                processors' queue locks, and shall otherwise wake it where it is.";
            attribute :>> criterion = "At two processors or more, a reader blocked
                in channel_write_read and free to move, woken by a Sync write from
                another processor, answers every time and is moved in at least one
                of 8 rounds; a reader pinned to processor 1 woken from processor 0
                answers on processor 1 in each of 8 rounds (the `sync` line).";
            attribute :>> parent = ("H.SCHED.4", "H.SCHED.1");
            attribute :>> unit = ("sched::wake_with", "sched::wake_onto",
                "sched::may_place", "sched::asleep_at_home",
                "sched::task::Task::holds_slots");
        }
        requirement <'L.sched.30'> theWayOutReadsTheWord : ItemLowLevel {
            attribute :>> statement = "attention_due, asked at every way out to ring 3
                on every architecture -- a call's, an interrupt's, a fault's, and the
                last look of the personality's return_to_user -- with interrupts masked,
                shall read the running task's pending-work word once, clear STOP and
                SIGNAL in it if they are set before anything reads what they stand
                for, and ask the personality's needs_attention when END, STOP, SIGNAL
                or TRACE was set; FILTERED shall not be read there.";
            attribute :>> criterion = "A program of three threads stopped by its stop
                alone, which posts STOP and no signal, has both counting threads park
                and stay still, and runs again after SIGCONT (stage 7's stop check,
                its second stop); every boot, test-shell, test-threads, test-vfs and
                test-init pass with the self-checks on, where L.sched.33's look runs
                at every clear word.";
            attribute :>> parent = ("H.OBJ.9", "H.TRAP.4");
            attribute :>> unit = ("trap::attention_due", "sched::work::look",
                "sched::work::wants_attention", "sched::work::with_running");
        }
        requirement <'L.sched.31'> notifyPostsBeforeItWakes : ItemLowLevel {
            attribute :>> statement = "notify shall post its bits to the task's word
                with a Release read-modify-write before it wakes the task and
                interrupts its processor; post and post_own shall only set bits; and
                only the task's own look shall clear any, with an Acquire
                read-modify-write, and END never.";
            attribute :>> criterion = "The stop check's second stop, posted by
                notify's half under the list's lock and woken after it, parks both
                counting threads; with the post removed the same check fails by
                name. The loom model of a post against the task's clear and read
                finds the state read or the bit kept, and its control, the clear
                after the read, finds neither (`cargo xtask loom`).";
            attribute :>> parent = ("H.OBJ.9", "H.SCHED.1");
            attribute :>> unit = ("sched::work::notify", "sched::work::wake_posted",
                "sched::work::post", "sched::work::post_own", "sched::work::has_end",
                "sched::task::Task::work");
        }
        requirement <'L.sched.32'> aWakeReadsStateUnderTheHomeLock : ItemLowLevel {
            attribute :>> statement = "A wake shall read its target's state only
                under the run-queue lock of the processor that owns the target, after
                reading which processor that is again under that lock, with no way
                out before it that reads the state without the lock.";
            attribute :>> criterion = "At two processors or more, a blocker that holds
                its own run-queue lock while a poster on another processor reaches its
                wake, then marks itself blocked and lets the lock go, is no longer
                blocked once the poster's wake has returned (the `wakerow` line); with a lock-free exit in wake_at_home the
                check fails by name.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("sched::wake_at_home", "sched::wake_onto");
        }
        requirement <'L.sched.33'> aClearWordHidesNoWork : ItemLowLevel {
            attribute :>> statement = "With the self-checks on, attention_due shall
                ask needs_attention at a clear word too, and audit shall stop the
                machine with FX-0520 when it answers yes while no poster is between
                its write and its post and the word is still clear; every poster
                shall bracket its write and its post in a Posting.";
            attribute :>> criterion = "Every boot, test-shell, test-threads, test-vfs
                and test-init reach their marker with the look on; with enter_stop's
                STOP to the other tasks removed, test-threads stops with FX-0520's
                message.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("sched::work::audit", "sched::work::posting",
                "sched::work::set_auditing", "sched::work::auditing");
        }
        requirement <'L.sched.34'> theWakeHookIsStageNinesAlone : ItemLowLevel {
            attribute :>> statement = "The hook in notify shall be armed only by stage
                9's checks, record which check armed it and the task it watches, be
                disarmed by that check whatever the check found, and be required
                disarmed before the boot's marker, with F-60's leave hook, which
                stops with FX-0908 naming the check if it is not.";
            attribute :>> criterion = "The `wakerow` line, then the marker; with the
                check's disarm removed the boot stops with FX-0908's message naming
                the wake row.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("sched::work::arm", "sched::work::disarm",
                "sched::work::armed_by", "sched::work::hook_armed_by");
        }
        requirement <'L.sched.35'> theCoreTellsEveryTaskOfAnEnd : ItemLowLevel {
            attribute :>> statement = "end_record shall store the process's end and
                then, after a SeqCst fence, post END to every task its list holds,
                waking each and interrupting its processor, with nothing allocated
                and no task let go under the list's lock; list_task shall list a
                task before it can run, failing its start with AllocError when the
                list cannot grow.";
            attribute :>> criterion = "A program past its cgroup's memory.max is
                OOM-killed and ends (the `cgroups` line's OOM case, which no signal
                reaches first); with end_record's post removed it never ends and
                the check fails by name.";
            attribute :>> parent = ("H.OBJ.9", "H.QUOTA.6");
            attribute :>> unit = ("object::process::Process::end_record",
                "object::process::Process::list_task",
                "object::process::Process::post_to_tasks",
                "object::process::Process::with_tasks", "object::process::Exit::record",
                "object::process::Process::take_listed", "object::process::take_batch",
                "object::process::next_live");
        }
        requirement <'L.sched.40'> aPostIsFencedBeforeItsWake : ItemLowLevel {
            attribute :>> statement = "wake_posted, the wake of every notify and of
                post_to_tasks, shall make a SeqCst fence after the bits are posted
                and before the wake reads the task's state, paired with a trusting
                wait's fence after it stores BLOCKED, so that a wait that reads the
                bit itself is found blocked or reads it.";
            attribute :>> criterion = "A thread blocked in channel_write_read is
                ended by its process's kill (the `wrread` line); with that wait
                reading no END it stays blocked and the check fails by name. The
                loom model of an end posted against a waiter listed and not yet
                blocked finds no lost wake (`cargo xtask loom`).";
            attribute :>> parent = ("H.OBJ.9", "H.OBJ.11");
            attribute :>> unit = "sched::work::wake_posted";
        }
        requirement <'L.sched.54'> everyResumeGoesThroughTheRestore : ItemLowLevel {
            attribute :>> statement = "Every switch to a task with user state shall
                restore it through switch_user_state, which shall tell the save
                whether the outgoing task leaves blocked, so that a state saved only
                in part is reset before the task runs, on every path that resumes
                it; the reset alone clears the mark.";
            attribute :>> criterion = "A program blocked in a native call is woken
                by a message, by its peer's close and by a message with a signal
                pending, each through its own path to choose_next, and reads the
                reset state each time (the `vectors` line, x86-64 and ARMv7-A).";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("sched::switch_user_state", "sched::with_own_user_state");
        }
    }

    package Preemption {
        doc /* src/kernel/src/sched/preempt.rs and src/kernel/src/sched/mod.rs:
             * the preemption count every crate::sync::SpinLock raises, kept in
             * each processor's record without a locked operation
             * (docs/OPAQUE-KERNEL.md §9.8, 2b; the consultant's OK IF of
             * 2026-10-02, condition 2). */

        requirement <'L.sched.20'> theCountStaysWithItsProcessor : ItemLowLevel {
            attribute :>> statement = "A preemption-disabling lock, and a raise by
                hand, shall raise and lower the preemption count and, for a lock,
                the locks held of the processor its holder runs on, in that
                processor's record and without a locked operation, so that no
                update is lost to an interrupt and none lands on another
                processor's record: on x86-64 in one GS-relative instruction, with
                every entry that can reach it running on the kernel's GS base; on
                AArch64 and ARMv7-A with every exception that can take a lock
                masked; and a report reading another processor's count shall read
                each of the count and the locks held whole.";
            attribute :>> criterion = "At two processors or more, two tasks per
                processor, preempted by the timer and moving between processors,
                take and let go of a lock 100000 times each with raises by hand
                among them and move at least once (the `preempt` line). The
                stress provokes a lost or misplaced update; what detects one is
                FX-0503's test at the release, which stops the boot when the locks
                held or the count it lowers is already zero, and, behind it, every
                processor's count and locks held read zero there by a task that
                holds nothing. The three atomicity controls (x86-64's record found
                before the add, the Arm mask removed on aarch64 and on armv7a at
                two processors) each stop the boot on FX-0503's held-zero
                message.";
            attribute :>> parent = ("H.SCHED.1", "H.SCHED.2");
            attribute :>> unit = ("sched::preempt::raise", "sched::preempt::lower",
                "sched::preempt::Preempt::disable", "sched::preempt::preempt_disable",
                "sched::preempt::PreemptState::note", "sched::preempt::preempt_count",
                "sched::preempt::locks_held", "smp::record", "arch::percpu::this_cpu_add",
                "arch::x86_64::cpu::gs_xadd");
        }
        requirement <'L.sched.21'> aCountThatCannotBeKeptStops : ItemLowLevel {
            attribute :>> statement = "The scheduler shall stop the machine
                (FX-0503), naming the site that last raised the count, when it is
                asked to switch with its processor's count raised, and when a
                release finds the count, or for a lock the locks held, already at
                zero, having put the word back first; a release that brings the
                count to zero shall make the decision an interrupt deferred, if
                interrupts are on. A wait's last look that blocks lowers its hand
                raise with interrupts masked, inside its own switch, and leaves the
                decision to that switch.";
            attribute :>> criterion = "The release's cover test refuses a word whose
                count, or for a lock whose locks held, is zero, and passes one that
                covers the release (the stage-5 checks). That held-zero test is the
                detector L.sched.20's stress relies on; the negative controls of a
                task that sleeps holding a lock and of a release with nothing raised
                each stop the boot on FX-0503's message. The blocking last look's
                own lowering has two controls of its own: one that lowers nothing
                stops the boot on FX-0503's switch with the count raised, and one
                that lowers twice on its release whose count was already zero.";
            attribute :>> parent = ("H.SCHED.1", "H.SCHED.2");
            attribute :>> unit = ("sched::preempt::preempt_enable",
                "sched::preempt::Preempt::enable", "sched::preempt::enable_from",
                "sched::preempt::unmatched", "sched::preempt::covers",
                "sched::preempt::decide_deferred", "sched::schedule_from",
                "sched::preempt::enable_for_switch", "sched::switch_from",
                "sched::block_ending_hold");
        }
        requirement <'L.sched.22'> mayBlockReadsItsOwnCount : ItemLowLevel {
            attribute :>> statement = "may_block shall answer true only when the
                scheduler runs, interrupts are on, a task is running, and the count
                of the processor it runs on, read so that the processor and its
                count are one processor's (one instruction on x86-64, interrupts
                masked on Arm), is zero.";
            attribute :>> criterion = "A task holding nothing may block; holding a
                preemption-disabling lock, or with the count raised by hand, it may
                not; once it lets go it may again (the stage-5 checks).";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("sched::may_block", "sched::preempt::count_here",
                "arch::percpu::this_cpu_read", "arch::x86_64::cpu::read_gs_at");
        }
        requirement <'L.sched.23'> aFailedTryLeavesTheCount : ItemLowLevel {
            attribute :>> statement = "A try_lock of a preemption-disabling lock
                that fails shall leave its processor's count and locks held as it
                found them, and with interrupts masked shall not switch; one that
                succeeds shall raise both by one until its guard drops; and
                ferrix_sync::SpinLock::try_lock_manually shall take no ticket when
                it fails.";
            attribute :>> criterion = "With a lock held, a try_lock of it with
                interrupts on and with them masked leaves the count and the locks
                held unchanged and, masked, the task's switches unchanged; a
                try_lock_manually of a held plain lock fails and changes neither;
                a successful try_lock raises both by one until it drops (the
                stage-5 checks, and ferrix-sync's host test of try_lock_manually).";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("sched::preempt::Preempt::disable",
                "sched::preempt::Preempt::enable");
        }
        requirement <'L.sched.95'> theDeadAreFreedWithNoProcessorIdle : ItemLowLevel {
            attribute :>> statement = "A task that ends while more than REAP_AHEAD dead
                tasks wait for the reaper, and that may block, shall first free up to
                REAP_BATCH of them, so that the dead waiting never number more than
                REAP_AHEAD plus one for each processor ending a task, whether or not
                any processor runs its idle task.";
            attribute :>> criterion = "At boot (the stage-5 checks): with every other
                processor held by a spinner and this one kept busy, 256 tasks made
                and ended here one after another leave at most REAP_AHEAD plus the
                processors' count waiting; with the ending task's reap removed, the
                count grows past it and the check fails.";
            attribute :>> parent = "H.SCHED.16";
            attribute :>> unit = ("sched::reap_ahead", "sched::exit_counted");
        }
    }

    package RunningTask {
        doc /* src/kernel/src/sched/borrow.rs and src/kernel/src/sched/mod.rs:
             * the running task borrowed from the processor's record
             * (docs/OPAQUE-KERNEL.md §9.8, 2a; the consultant's OK IF of
             * 2026-10-02, condition 1, and Q1). */

        requirement <'L.sched.24'> theRecordNamesTheRunningTask : ItemLowLevel {
            attribute :>> statement = "Whenever interrupts are on, each processor's
                record shall name, as Arc::as_ptr, the task its run queue's current
                holds; the scheduler shall write the two together, on that
                processor with interrupts masked and its queue lock held, at the
                boot task's adoption, at an idle task's installation and at every
                switch, and nowhere else.";
            attribute :>> criterion = "From boot to the end of stage 5, an audit
                compares the record with the queue's current, read under its lock,
                once the boot task is adopted, as each processor's idle task first
                runs, in the incoming context of every switch and at every
                interrupt exit, and finds no mismatch at any site; and two tasks per
                processor, yielding to each other, resume after a switch 10000
                times, each time with the record naming the queue's current and the
                task itself (the `borrow` line). Removing the record's write at the
                adoption, at the idle installation or at the switch stops the boot
                on that site's message.";
            attribute :>> parent = ("H.SCHED.1", "H.SCHED.6");
            attribute :>> unit = ("sched::set_current", "sched::adopt_boot_task",
                "sched::enter_idle", "sched::choose_next",
                "sched::borrow::RunningSlot::set", "sched::audit_here",
                "sched::borrow::audit", "sched::borrow::end_audit");
        }
        requirement <'L.sched.25'> theRunningTaskIsLentWithoutTheLock : ItemLowLevel {
            attribute :>> statement = "with_current shall lend the running task
                only to a closure, by a reference its result cannot name, read from
                the processor's record so that the processor and its record are
                one processor's (one instruction on x86-64, interrupts masked on
                Arm); current() shall answer the running task as an Arc of the
                caller's own without taking the run queue's lock; and before the
                records are kept both shall answer none.";
            attribute :>> criterion = "Each of the borrow check's 10000 resumptions
                borrows the running task and finds the task itself, and every
                audit's borrowed pointer equals the queue's current (the `borrow`
                line); every boot, shell and bench gate runs on current() made
                this way.";
            attribute :>> parent = ("H.SCHED.1", "H.SCHED.6");
            attribute :>> unit = ("sched::borrow::with_current",
                "sched::borrow::current_arc", "sched::borrow::current_ptr",
                "sched::borrow::running", "sched::current");
        }
    }

    package TheSwitchWords {
        doc /* src/kernel/src/sched/mod.rs: the words the switch and every way
             * out write, by plain loads and stores where only their own
             * processor writes them (docs/OPAQUE-KERNEL.md §9.8, 2f; the
             * consultant's OK IF of 2026-10-02, condition 6). */

        requirement <'L.sched.50'> theIdleBitChangesOnlyWithTheIdleTask : ItemLowLevel {
            attribute :>> statement = "At a switch, choose_next shall write the
                processor's bit in IDLE only when the bit differs from whether the
                incoming task is the processor's idle task, so that the bit is clear
                whenever another task runs.";
            attribute :>> criterion = "Two tasks a processor, yielding to each other
                and every 16th turn sleeping, so that a processor whose tasks both
                sleep switches from its idle task at a timer interrupt's exit, resume
                after a switch 10000 times between them and never read their
                processor as idle (the `idle` line); with the switch's clear skipped,
                the check fails by name.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("sched::set_idle_at_switch", "sched::set_idle");
        }
        requirement <'L.sched.51'> aCountedMoveIsReadWithItsJob : ItemLowLevel {
            attribute :>> statement = "note_moved shall count a move with a SeqCst
                increment of MOVES after the move has stored the process's new job;
                regroup_current shall load MOVES with Acquire, compare it with its processor's
                RUNNING_SEEN by a load, store RUNNING_SEEN only when they differ, and
                then make a SeqCst fence before it reads the job, so that a way out
                that sees a move counted reads the job it moved to.";
            attribute :>> criterion = "The loom model of a move against a way out
                finds every way out that sees the move counted reading the new job,
                and its control, the count's load Relaxed and the fence removed,
                finds one reading the old job (`cargo xtask loom`, src/tests/loom).";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("sched::note_moved", "sched::regroup_current");
        }
        requirement <'L.sched.52'> aWayOutSwapsOnlyItsOwnPostedRequest : ItemLowLevel {
            attribute :>> statement = "take_resched shall read its own processor's
                reschedule request by a load, and clear it by a swap only when the
                load finds it set.";
            attribute :>> criterion = "A task spawned onto its spawner's processor
                and a task woken onto the waker's, by a spinning task that leaves
                through no interrupt exit, each run within 200 ms (stage 5); with
                take_resched loading another processor's request, the check fails
                by name.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = "sched::take_resched";
        }
    }

    package DirectSwitch {
        doc /* src/kernel/src/sched/direct.rs, sched/queue.rs and
             * src/lib/kernel/sched-fair: the direct switch of channel_write_read's fast
             * path (docs/OPAQUE-KERNEL.md §9.7, part 1). */

        requirement <'L.sched.55'> handOverIsTheGeneralSequence : ItemLowLevel {
            attribute :>> statement = "RunQueue::hand_over shall leave the queue, the
                arriving entity and the leaving entity's state exactly as enqueue,
                set_slice_ns, remove_curr and pick_next leave them on a queue with
                one entity running and nothing waiting.";
            attribute :>> criterion = "hand_over_is_the_general_sequence: 200,000
                random queues, weights and lags at and past the clamp, virtual times
                either side of the wrap, every field compared; its control changes
                one field of the result and fails.";
            attribute :>> parent = "H.SCHED.13";
            attribute :>> unit = "sched::queue::CpuQueue::hand_over";
        }
        requirement <'L.sched.56'> onlyWhenNothingElseCouldBePicked : ItemLowLevel {
            attribute :>> statement = "direct::begin shall take this processor's
                run-queue lock only if it is free, and decline unless the task to
                switch to has this processor as its home and holds both its slots
                (T11), nothing waits in the fair class and no sleeper is due (T12),
                and neither task has END posted (T13), leaving the queue unchanged
                when it declines.";
            attribute :>> criterion = "Stage 9's cases 9, 10 and 14 reach T11, T12
                and T13 with the fast path on and get the general path's results;
                each test's control fails its case (the `fastcase` line).";
            attribute :>> parent = "H.SCHED.13";
            attribute :>> unit = ("sched::direct::begin",
                "sched::queue::CpuQueue::sleeper_due");
        }
        requirement <'L.sched.57'> theTailIsChooseNexts : ItemLowLevel {
            attribute :>> statement = "The direct switch shall make FX-0503's check
                and then the switch through switch_chosen, the tail choose_next
                makes after its pick, switch_to and finish_switch.";
            attribute :>> criterion = "choose_next and Direct::switch both call
                switch_chosen (review); every boot runs choose_next through it, and
                the fast path's trips through Direct::switch (the counts line).";
            attribute :>> parent = "H.SCHED.13";
            attribute :>> unit = ("sched::switch_chosen", "sched::direct::Direct::switch",
                "sched::require_preemption_on");
        }
        requirement <'L.sched.58'> theAssertsStopTheMachine : ItemLowLevel {
            attribute :>> statement = "Direct::hand_over shall stop the machine with
                FX-0532 when either task is the processor's idle task (A4), and with
                FX-0530 when the task handed over to is running here, queued, not
                blocked, or not the pick (A1).";
            attribute :>> criterion = "A record planted on a running task stops the
                machine with FX-0530 (the A1 control).";
            attribute :>> parent = "H.SCHED.13";
            attribute :>> unit = "sched::direct::Direct::hand_over";
        }
        requirement <'L.sched.59'> theReplyCellIsTheTasksOwn : ItemLowLevel {
            attribute :>> statement = "A task's reply cell shall be filled only by the
                commit that hands the processor to it, under its home's run-queue
                lock while it is asleep, and emptied only by the task itself.";
            attribute :>> criterion = "ipc-equiv's case 1 and 2 with the fast path
                on: every length 0 to 24 and 1,000 trips in order.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("sched::task::Task::fill_reply",
                "sched::task::Task::take_reply");
        }
        requirement <'L.sched.60'> theFrameTailLooksAtEverything : ItemLowLevel {
            attribute :>> statement = "nothing_due_here shall answer true only when
                the running task's pending-work word is clear, no decision is asked
                of this processor and no move between jobs is unseen.";
            attribute :>> criterion = "Case 14, the continuation's kill and case 15's
                signal leave through the general branch and act on their bit.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("sched::nothing_due_here", "sched::work::peek");
        }
        requirement <'L.sched.61'> theCountsAreTheKernels : ItemLowLevel {
            attribute :>> statement = "The fast path shall count its trips, parks and
                each test's declines per processor, readable by no program, printed
                as the shell exits; a park a boot check makes through the receive
                half itself (stage 9's parked-block case) is taken back by that
                check, so the counts are the programs' own.";
            attribute :>> criterion = "test-ipc-equiv requires trips with the fast
                path on and every count zero with it off, stage 9's parked-block
                case having taken back the parks it made.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("sched::direct::count", "sched::direct::counts",
                "fastpath::report_counts");
        }
        requirement <'L.sched.62'> theLastLooksHook : ItemLowLevel {
            attribute :>> statement = "The fast path's hook before T13 shall post END
                to the caller only while stage 9's case 14 has armed it on that task,
                and cost one load otherwise; a boot with it armed after stage 9 stops
                with FX-0908.";
            attribute :>> criterion = "Case 14 answers EINTR within the bound; with
                T13 removed it stays blocked past it (the T13 control).";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = "sched::work::fast_path_hook";
        }
        requirement <'L.sched.63'> aTaskSlotIsOneAtomicCell : ItemLowLevel {
            attribute :>> statement = "Each of a task's two slots shall be held in
                one atomic cell: taking it shall swap the cell with empty and answer
                the node only to that swap, giving it back shall be a
                compare-exchange from empty that stops the machine with FX-0534 when
                the cell is not empty, asking whether it is held shall be one load,
                and the task's drop shall free a node the cell holds; nothing shall
                be allocated by any of them.";
            attribute :>> criterion = "The loom model of the cell finds no
                interleaving of two takers, a give-back and a look where two
                holders have the node or a node is lost, and fails both its controls
                (a take by a load and a store; a give-back by a plain store); a slot
                taken apart and put together serves a queue as the same node (the
                ferrix-sched-fair host test).";
            attribute :>> parent = ("H.SCHED.13", "H.FAIL.1");
            attribute :>> unit = ("sched::task::Task::take_run_slot",
                "sched::task::Task::return_run_slot", "sched::task::Task::keep_entity",
                "sched::task::Task::take_sleep_slot",
                "sched::task::Task::return_sleep_slot", "sched::task::Task::holds_slots",
                "sched::task::Task::holds_sleep_slot");
        }
        requirement <'L.sched.64'> theDirectSwitchFoldsItsJobLoad : ItemLowLevel {
            attribute :>> statement = "The direct switch shall change the processor
                load of a job both its tasks are counted in by the net of the peer's
                join and the caller's leave in one adjustment, and otherwise make the
                peer's join and then the caller's leave as the general path does; a
                direct switch that does not reach the caller's leave shall make the
                peer's join alone.";
            attribute :>> criterion = "Stage 9's job cases, with the fast path off
                and on: after trips between a caller and an echo in one job with
                equal and with unequal weights, in two sibling jobs, and in a job
                and its child, each job's load is what its tasks' words count plus
                what its busy children add, a child adds exactly while busy, every
                load reads 0 once both are gone, and with the fast path on the first
                two fold and the last two do not; controls: the net's sign flipped,
                and a fold at a job the two do not share, each fail it. The last
                clause, a direct switch that does not reach the caller's leave, is
                shown by review: every way out that drops PendingJoin unsettled ends
                in FX-0530, which no passing boot reaches.";
            attribute :>> parent = ("H.SCHED.13", "H.SCHED.3");
            attribute :>> unit = ("sched::queue::CpuQueue::hand_over",
                "sched::direct::Direct::hand_over", "sched::task::Task::join_word",
                "sched::task::Task::leave_word", "sched::queue::PendingJoin::drop");
        }
        requirement <'L.sched.65'> theDirectSwitchWeighsWithThePeerCounted : ItemLowLevel {
            attribute :>> statement = "The direct switch shall give the peer, and the
                running task under account's rule, the weights effective gives with
                the peer counted in its job's load.";
            attribute :>> criterion = "Stage 9's weight case, with the fast path off
                and on: a caller of trips in one job with an echo of another weight
                runs, after its last trip, at the weight effective gives with the
                echo counted, or at the one its job's share gives it as the loads
                stand when a charge since has moved it; control: the hand-over's
                weights computed without the peer counted fail it.";
            attribute :>> parent = ("H.SCHED.13", "H.SCHED.3");
            attribute :>> unit = ("sched::queue::CpuQueue::hand_over",
                "sched::queue::CpuQueue::account_with", "sched::task::Task::effective_weight_with",
                "object::quota::effective_with");
        }
        requirement <'L.sched.66'> theFoldIsPlannedBeforeTheCharge : ItemLowLevel {
            attribute :>> statement = "The direct switch shall plan a fold of its job
                loads from the peer's join and the caller's word before the charge,
                only at a job both are counted in, and shall settle it once the
                caller's leave has answered: by the net in one adjustment when the
                leave answered as planned, and otherwise by the peer's join and then
                the caller's leave, each in its own job.";
            attribute :>> criterion = "ferrix-sched-fair's host test drives the kernel's
                plan and settlement against a model of quota::adjust over 40,000
                random job trees of depth 1 to 8 with siblings and nested jobs, the
                two tasks in one job, siblings, nested either way and apart, equal
                and unequal weights, and finds every job's load and contribution
                and both weights equal to the general sequence's; with a fold
                planned at a job the two do not share, it fails.";
            attribute :>> parent = "H.SCHED.13";
            attribute :>> unit = ("sched::direct::Direct::hand_over",
                "sched::queue::PendingJoin::settle");
        }
        requirement <'L.sched.71'> aWaitsLastLookIsNotPreempted : ItemLowLevel {
            attribute :>> statement = "No switch shall take place between a wait's
                setting its task blocked and the end of its last look (WaitQueue's
                single-queue and many-queue waits); an interrupt's exit, or a
                preempt_enable inside the look, shall leave the decision it asks for
                pending until the look is over, and that decision shall then be
                made. On a wait that blocks, its block's switch is that decision,
                the count lowered with interrupts masked for it.";
            attribute :>> criterion = "Stage 5's last-look case: a waiter that
                drains its own entry while runnable, meets its condition and invites
                a switch inside its last look, once by a lock's release and once by
                a timer interrupt's exit, finishes its wait, stays queued and
                unswitched through the look, and has the decision made after it;
                with the hold off, or on main's code without it, the case fails with
                its sentence for a waiter left blocked on no run queue (F-69's
                controls). Its blocking sub-case: a waiter whose last look invites a
                decision by a lock's release and fails stays queued, unswitched and
                blocked through the look, is found blocked off its run queue by its
                waker, and returns true with the count at zero and interrupts on;
                with the hold off and the drained cases left out, it fails with its
                own sentence.";
            attribute :>> parent = ("H.OBJ.11", "H.SCHED.2");
            attribute :>> unit = ("sched::wait::LastLook::hold",
                "sched::wait::LastLook::drop",
                "sched::wait::WaitQueue::wait_sliced",
                "sched::wait::WaitQueue::wait_on_any",
                "sched::wait::LastLook::block", "sched::block_ending_hold");
        }
        requirement <'L.sched.72'> aParkedBlocksLastLookIsNotPreempted : ItemLowLevel {
            attribute :>> statement = "No switch shall take place between the receive
                half's park setting its task blocked and the end of its last look at
                END; an END posted while the task still ran shall be found by that
                look and end the park.";
            attribute :>> criterion = "Stage 9's parked-block case: a reader that posts
                END to itself through the poster, asks for a decision and parks on an
                empty end comes back woken in every try, at least one try parking with
                the decision still pending; with the hold taken after the park, or off,
                the case fails with its sentence for a reader left blocked on no run
                queue (F-69's controls).";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("syscall::native::park_for_reply",
                "object::channel::Endpoint::park", "sched::direct::block_parked",
                "sched::wait::LastLook::block", "sched::block_ending_hold");
        }
        requirement <'L.sched.73'> aWaiterDrainedAsItIsListedIsWoken : ItemLowLevel {
            attribute :>> statement = "A single-queue wait shall mark its task
                blocked under the queue's lock, with the listing, so that a wake
                which drains the entry makes the task runnable even when the change
                it brings does not yet meet the wait's condition, and the task lists
                itself again for the next wake.";
            attribute :>> criterion = "Stage 5's listed-drain case: a trusting waiter
                held by the wait's meet just after its listing, on a processor of its
                own, whose queue the checker wakes there with the condition false,
                then wakes again with it true, finishes its wait; with the mark moved
                after the list's lock, the case fails with its sentence for a waiter
                left blocked on no list (pipe-epipe's control).";
            attribute :>> parent = ("H.OBJ.11", "H.SCHED.2");
            attribute :>> unit = ("sched::wait::WaitQueue::wait_sliced",
                "sched::wait::listed_meet");
        }
        requirement <'L.sched.74'> aWakeTellsItsMarks : ItemLowLevel {
            attribute :>> statement = "Every wake of a wait queue shall tell each
                mark listed on it exactly once, after waking the tasks it drained; a
                mark taken off shall be told by no wake that takes the hook lock after
                the taking-off returns; a queue dropped shall tell each mark still
                listed that it is gone, and wake nothing.";
            attribute :>> criterion = "Stage 5's told case: a mark hooked on a queue is
                told once by each of three wakes and its number listed each time, is
                told by no wake once unhooked, and is told it is gone, listed and
                with its list's queue not woken, when a queue it is hooked on is
                dropped; with the telling taken out of the wake, the unhook leaving
                the entry, or the drop telling nothing, the case fails with its
                sentence.";
            attribute :>> parent = ("H.OBJ.11", "H.SCHED.2");
            attribute :>> unit = ("sched::wait::WaitQueue::hook",
                "sched::wait::WaitQueue::unhook",
                "sched::wait::WaitQueue::wake_all_with",
                "sched::wait::WaitQueue::drop");
        }
        requirement <'L.sched.75'> aMarkHookedIsToldByLaterWakes : ItemLowLevel {
            attribute :>> statement = "A mark listed by hook shall be told by every
                wake whose waiter-lock section begins after hook returns; the count of
                listed marks shall be stored and read only under the waiter lock.";
            attribute :>> criterion = "Stage 5's published case: a waker on a processor
                of its own, held by the wake's meet just before its waiter-lock
                section, tells a mark the checker hooks on the queue while it is
                held; with the count read before the meet, outside the waiter lock,
                the case fails with its sentence for a mark the wake did not tell.";
            attribute :>> parent = ("H.OBJ.11", "H.SCHED.2");
            attribute :>> unit = ("sched::wait::WaitQueue::hook",
                "sched::wait::WaitQueue::wake_all_with",
                "sched::wait::hook_meet");
        }
        requirement <'L.sched.76'> aMarkIsListedOnceAndWakesItsList : ItemLowLevel {
            attribute :>> statement = "A mark told of a wake shall put its number on
                its ready list at most once until the list's taker clears it, shall
                flag the list overflowed rather than grow it when it is full, and
                shall wake the list's queue when told it was woken unless a mark of
                the list has woken it since the list was last taken, never when told
                it is gone.";
            attribute :>> criterion = "Stage 5's listed-once case: a mark told twice
                before its taker clears it is listed once and wakes its list's queue
                once; a second mark of the same list told before the next take wakes
                it no more, and the first mark told after a take wakes it again; a
                list one place short of its marks, all told, is flagged overflowed
                and holds as many as it had room for; and the told case's drop wakes
                nothing. With the listing ignoring the mark's state, the overflow flag
                not set, or the list's queue not woken, the case fails with its
                sentence.";
            attribute :>> parent = ("H.OBJ.11", "H.SCHED.2");
            attribute :>> unit = ("sched::wait::Mark::tell",
                "sched::wait::Mark::clear", "sched::wait::ReadyList::take",
                "sched::wait::ReadyList::reserve");
        }
        requirement <'L.sched.80'> aReturnToUserModeEndsKernelTime : ItemLowLevel {
            attribute :>> statement = "On a boot that counts kernel time (its clock's
                counter the invariant TSC or CNTVCT), a task's stint in the kernel
                shall end at its return to user mode from a system call (SYSRET on
                x86-64, the exception return on AArch64 and ARMv7-A) and from an
                interrupt or an exception through the trap dispatcher; the time it
                then runs in user mode shall not be counted as kernel time. Not
                covered: a program's first entry, which its first page fault's
                return follows within microseconds, so that no check can see the
                first entry's own stamp; a fork child's first return; and
                rt_sigreturn's and a 32-bit execve's way out on x86-64; all four take
                the same stamp, unchecked. Kernel time read from another processor
                counts up to the running task's last return to user mode or its
                processor's last decision, which a task alone on a processor with
                no tick may not make for as long as it runs.";
            attribute :>> criterion = "Stage 7's user-time case, on every boot that
                counts kernel time: programs spinning 60,000,000 times after their
                start, after a getppid, after a page fault, and two taking turns on
                one processor each cut by an interrupt in user mode, are each
                charged under a quarter of their runtime as kernel time, read after
                each has exited, in one of three runs (a host stall is charged to
                what it stopped); with the system call return's or the dispatcher's
                return stamp taken out, the case fails with its sentence for user
                time charged as kernel time.";
            attribute :>> parent = "H.SCHED.14";
            attribute :>> unit = ("sched::vtime::leaving", "sched::vtime::settle",
                "sched::vtime::init", "trap::dispatch",
                "arch::x86_64::syscall::ferrix_syscall_entry");
        }
        requirement <'L.sched.81'> aSystemCallIsKernelTime : ItemLowLevel {
            attribute :>> statement = "On a boot that counts kernel time, a task's
                system call from user mode shall be counted as kernel time from its
                entry (x86-64's SYSCALL entry after the channel_write_read fast
                path's branch; on AArch64 and ARMv7-A the trap dispatcher's entry)
                to its return to user mode, and the kernel time a task makes shall
                be given to that task at its processor's next decision. Not
                counted, as user time: the span from a call's or a trap's last
                scheduling decision (any decision on its processor during it: a
                switch back after it blocked or was preempted, a decision that kept
                it running, its own read of its CPU time) to its return; on x86-64, the three native calls that block
                (channel_write_read, object_wait_one, port_wait) up to their
                decision; the fast path's own time in the kernel, with
                ferrix.fastpath=on, outside the present configuration, which counts
                as user time; the instructions of the entry and exit stubs before
                the entry's stamp and after the return's; and the paranoid entries
                (a non-maskable interrupt, a machine check, a debug exception from
                user mode), the ARMv7-A VFP first-use return and the x86-64 CSTAR
                refusal, which never reach a stamp.";
            attribute :>> criterion = "Stage 7's system-call case, on every boot
                that counts kernel time: a program making 50,000 getppid calls in a
                loop is charged at least half its runtime as kernel time, in one of
                three runs; with
                x86-64's entry stamp taken out, or on AArch64 and ARMv7-A the
                dispatcher's, the case fails with its sentence.";
            attribute :>> parent = "H.SCHED.14";
            attribute :>> unit = ("sched::vtime::entered", "sched::vtime::settle",
                "sched::vtime::direct_switch",
                "arch::x86_64::syscall::ferrix_syscall_entry",
                "arch::x86_64::syscall::general_entry",
                "arch::x86_64::syscall::frame_tail", "trap::dispatch");
        }
        requirement <'L.sched.82'> anExceptionIsKernelTime : ItemLowLevel {
            attribute :>> statement = "On a boot that counts kernel time, an
                exception taken in user mode shall be counted as kernel time from
                the trap dispatcher's entry to its return to user mode, with the
                exclusions of L.sched.81. An interrupt taken in user mode takes the
                same dispatcher stamp, unchecked.";
            attribute :>> criterion = "Stage 7's page-fault case, on every boot that
                counts kernel time: a program that maps 2,048 pages and touches each
                once is charged at least half its runtime as kernel time, in one of
                three runs; with the
                dispatcher's entry stamp taken out on x86-64, where system calls do
                not pass through it, the case fails with its sentence.";
            attribute :>> parent = "H.SCHED.14";
            attribute :>> unit = ("sched::vtime::entered",
                "sched::vtime::trap_entered", "trap::dispatch");
        }
        requirement <'L.sched.83'> switchesAreCountedByWhy : ItemLowLevel {
            attribute :>> statement = "Every switch away from a task shall count one
                involuntary switch for it when it is still runnable -- preempted,
                yielding, or a sleep that could not be filed -- and one voluntary
                switch otherwise; a decision that keeps the running task shall
                count none.";
            attribute :>> criterion = "Stage 7's switch case: two kernel tasks on one
                processor yielding to each other 20 times each, never blocking,
                count one voluntary switch, at their end, and every other switch
                away from them as involuntary, at least one, the two adding to how
                often each was switched to; a task that sleeps 10 times counts at
                least 10 voluntary and fewer than 10 involuntary; with the
                classification inverted, the case fails with its sentence.";
            attribute :>> parent = "H.SCHED.14";
            attribute :>> unit = ("sched::switch_chosen",
                "sched::task::Task::note_switched_out");
        }
        requirement <'L.sched.86'> aBlockedCallIsReadFromItsEntryFrame : ItemLowLevel {
            doc /* x86-64 only: on AArch64 and ARMv7-A the exception frame is
                 * pushed where the task last returned to user mode, no fixed
                 * place, and the view reads no call there (docs/DEBUG.md 1.1;
                 * BACKLOG p2-140). The fault case's -1 sp pc and a 32-bit
                 * program's int 0x80 frame are read the same way and are not
                 * claimed: no check drives them. */
            attribute :>> statement = "On x86-64, the view of a thread blocked in a
                system call made through the 64-bit entry shall report the call's
                number and six arguments, the program's stack pointer and its
                instruction pointer, as the entry saved them in the system call
                frame at the top of the task's kernel stack, read while the task is
                blocked and switched away under its run queue's lock; and the view
                of the reading thread itself shall report it running. On AArch64
                and ARMv7-A the view shall read no call, so that /proc answers
                EOPNOTSUPP rather than a call it did not read.";
            attribute :>> criterion = "test-procfs's debug step on x86_64: a thread
                blocked in read of a pipe reads back read's number, the descriptor,
                the buffer's address and the length, a stack pointer and a nonzero
                instruction pointer, and the reader's own file reads running; with
                the frame's first argument read from the second's place, the step
                fails with its sentence; on aarch64 and armv7a the step reads
                syscall as EOPNOTSUPP.";
            attribute :>> parent = "H.SCHED.15";
            attribute :>> unit = ("sched::inspect::call_of", "sched::inspect::sample",
                "sched::inspect::still", "sched::inspect::calls_are_visible",
                "sched::inspect::read_call", "sched::inspect::field",
                "arch::x86_64::inspect::entry_frame",
                "arch::aarch64::inspect::entry_frame", "arch::aarch64::inspect::x",
                "arch::armv7a::inspect::entry_frame", "arch::armv7a::inspect::r",
                "sched::task::Task::in_call");
        }
        requirement <'L.sched.87'> aBlockedChainIsReadFromItsSwitchFrame : ItemLowLevel {
            attribute :>> statement = "The view of a task that is blocked and switched
                away shall report its kernel call chain from the frame pointer and
                return address its switch frame holds, innermost first, following
                only records inside the task's own stack, each above the last and
                aligned, and only return addresses inside the kernel image, at most
                backtrace::MAX_FRAMES, each with the image's slide taken off.";
            attribute :>> criterion = "The task view case at boot on every
                architecture: a kernel task records its own chain and blocks; the
                view's chain of it ends in that chain, each address inside the
                image as it was linked; with the switch frame's frame pointer slot
                moved by a word, or the slide not taken off on a boot whose slide
                is not zero, the case fails with its sentence.";
            attribute :>> parent = "H.SCHED.15";
            attribute :>> unit = ("sched::inspect::chain_of", "sched::inspect::sample",
                "sched::inspect::still", "sched::inspect::walk",
                "sched::inspect::first_link", "sched::inspect::link_at",
                "backtrace::in_image", "backtrace::slide", "backtrace::image_len");
        }
        requirement <'L.sched.88'> aFreedStackReadsAsNothing : ItemLowLevel {
            attribute :>> statement = "A word of kernel memory read for the view shall
                be translated through the kernel's page tables under their lock and
                read through the direct map, so that a word of a stack that has
                been freed answers nothing and the read does not fault.";
            attribute :>> criterion = "The task view case at boot: a stack from vmap,
                read while mapped, then freed and read again, answers nothing; with
                the read made through the address itself, the boot stops on a
                kernel page fault.";
            attribute :>> parent = "H.SCHED.15";
            attribute :>> unit = ("mm::read_kernel_word", "sched::inspect::word_at");
        }
        requirement <'L.sched.89'> aWordThatIsNoRamReadsAsNothing : ItemLowLevel {
            attribute :>> statement = "A word of kernel memory read for the view whose
                translation names a frame that is not RAM shall answer nothing,
                without the read being made.";
            attribute :>> criterion = "The task view case at boot: a page of vmap
                mapped onto a physical address that is no RAM answers nothing; with
                the RAM test taken out, the boot stops on a kernel page fault on
                x86_64, where the direct map has no such address.";
            attribute :>> parent = "H.SCHED.15";
            attribute :>> unit = "mm::read_kernel_word";
        requirement <'L.sched.85'> aWokenTaskIsPlacedOnAnIdleProcessor : ItemLowLevel {
            attribute :>> statement = "A task made runnable on a processor that runs a
                task -- woken from another processor, or a sleeper whose time
                came on that processor -- shall be queued
                on an idle processor of the same work-stealing domain that it may
                run on, whose queue, under its own lock, runs its idle task with
                nothing waiting, and that processor shall be told to decide, the
                running task keeping its processor; where no such queue's lock can
                be had without waiting, the task shall be queued on the first
                processor as before. A pinned task, or one in a domain that does
                not steal work, is never so placed.";
            attribute :>> criterion = "Stage 5's placement case, with three processors
                or more and a stealing domain: a task homed on a processor that
                runs a pinned spinner, allowed there and on an idle one, woken
                from a third processor and, in a second case, at its sleep's end,
                runs on the idle processor within a second and the spinner counts
                no involuntary switch (three attempts each); with no candidate
                taken, with the target not told, with the sleeper path placing
                nothing, or with affinity ignored, the boot fails with the case's
                own sentence or stage 5's affinity check's.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("sched::place_on_idle", "sched::move_onto",
                "sched::placed_on_wake", "sched::wake_sleepers_placing",
                "sched::queue::CpuQueue::wake_sleepers");
        }
        requirement <'L.sched.90'> aWakeeIsPulledOntoItsWakersProcessor : ItemLowLevel {
            attribute :>> statement = "A task made runnable by a wake (not a
                synchronous one) that a task running inside a system call makes,
                with interrupts enabled, on a processor of a work-stealing domain,
                shall be moved onto that processor and queued there only when:
                the task is not pinned and may run there; its home is another
                processor of the domain; the wake is not wide (L.sched.91); no
                processor it may run on but its home is idle; and, under both queues' locks
                taken lower processor first, its home runs a task, it is asleep
                at home, its busy share (its runtime over a window of at least
                32 ms between two such looks, the first opened when it was made)
                has been measured, and the waker's processor's load average with
                the task's load (that share times its weight) added, times 100,
                is below its home's with that load taken away, times 108.
                Otherwise it shall be made runnable
                at home as before, an idle processor then taken by L.sched.85. A
                task moved shall be decided for at once, as a wake at home made on
                its own processor is. A wake by a sleep's end, an interrupt or a
                task outside a system call never moves a task so.";
            attribute :>> criterion = "Stage 5's wake-affine case, with three
                processors or more and a stealing domain: a task homed on a
                processor that runs a pinned spinner, allowed there and on the
                checking processor, woken from a call made there, runs there; the
                same task pinned at home, with its home idle, woken by its
                sleep's end, or busy for 40 ms of its life before it blocked,
                runs at home (three attempts each); ferrix-sched-class's host tests
                hold the load comparison; with the pull removed from the wake,
                with the pinned and affinity tests dropped, or with the task's
                load left out, the boot fails with the case's own sentence.";
            attribute :>> parent = ("H.SCHED.4", "H.SCHED.1");
            attribute :>> unit = ("sched::wake", "sched::pull_onto_waker",
                "sched::pull_target", "sched::idle_allowed", "sched::wake_onto",
                "sched::count_pulled", "sched::may_pull", "sched::refuses",
                "sched::task::Task::busy_share");
        }
        requirement <'L.sched.91'> aWideWakePullsNothing : ItemLowLevel {
            attribute :>> statement = "Each task that wakes another from a system
                call with interrupts enabled shall record it as its last wakee, its flip count halved
                first when more than a second has passed since it was last halved
                and raised by one when the wakee is not the last one; and a wake
                shall move nothing under L.sched.90 when the smaller of the
                waker's and the wakee's flip counts is at least the domain's
                processor count and the larger is at least that count times the
                smaller.";
            attribute :>> criterion = "Stage 5's wake-affine case: with the waker's
                flips set wide and the wakee's at the domain's processor count, the
                wake that is pulled otherwise runs the task at home (three
                attempts); ferrix-sched-class's host tests hold the bound and the
                halving; with the refusal ignored, the boot fails with the case's
                own sentence.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("sched::task::Task::record_wakee",
                "sched::task::Task::wakee_flips");
        }
        requirement <'L.sched.92'> aWaitEndsAtItsDeadline : ItemLowLevel {
            attribute :>> statement = "wait_until_deadline shall return false once
                the monotonic clock passes deadline while ready stays false, and
                true as soon as a wake finds ready true.";
            attribute :>> criterion = "Stage 5's deadline case: a wait 20 ms out
                whose condition stays false returns false after its deadline and
                within a second of it, and a wait 10 s out whose condition a waker
                makes true and wakes returns true within a second; with the
                deadline's test skipped for the check's own waiting task the
                first case fails with its sentence, found by the checker's own
                sleep (removed for every wait, the boot hangs at stage 3; the
                crash hold's 2 s rests on it, docs/LOGGING.md section 21.7,
                ledger 3537 and 3545).";
            attribute :>> parent = "H.OBJ.11";
            attribute :>> unit = ("sched::wait::WaitQueue::wait_until_deadline",
                "sched::wait::WaitQueue::wait_sliced");
        }
    }
}
