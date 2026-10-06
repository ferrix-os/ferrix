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
    }

    package Wakes {
        doc /* src/kernel/src/timer.rs and src/kernel/src/sched/mod.rs: the
             * timer's skipped arm, the clock, and the decisions a channel
             * round trip's wakes defer (docs/OPAQUE-KERNEL.md §9.4, items 1,
             * 2, 3 and 6; the consultant's conditions of 2026-10-02). */

        requirement <'L.sched.5'> aSkippedArmIsNeverLate : ItemLowLevel {
            attribute :>> statement = "timer::after shall keep, for each
                processor, the clock read after the hardware was armed plus the
                delay asked for, and shall leave a one-shot armed only when that is
                no later than the new deadline; otherwise it shall arm for the new
                one, so that no interrupt asked for is served later than the
                hardware's own one-tick rounding of a delay shorter than a tick.";
            attribute :>> criterion = "On the boot processor, a 2 ms one-shot asked
                for while a 1 s one is armed, and one asked for before a 1 s one,
                each fire within 200 ms of the 2 ms deadline (the `arm` line).";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("timer::after", "timer::stop", "timer::on_tick",
                "timer::armed_slot");
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
                wake, then marks itself blocked and lets the lock go, is woken within
                100 ms (the `wakerow` line); with a lock-free exit in wake_at_home the
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
                reset state each time (the `vectors` line).";
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
                interrupts are on.";
            attribute :>> criterion = "The release's cover test refuses a word
                whose count, or for a lock whose locks held, is zero, and passes one
                that covers the release (the stage-5 checks). That held-zero test is
                the detector L.sched.20's stress relies on; the negative controls of
                a task that sleeps holding a lock and of a release with nothing
                raised each stop the boot on FX-0503's message.";
            attribute :>> parent = ("H.SCHED.1", "H.SCHED.2");
            attribute :>> unit = ("sched::preempt::preempt_enable",
                "sched::preempt::Preempt::enable", "sched::preempt::enable_from",
                "sched::preempt::unmatched", "sched::preempt::covers",
                "sched::preempt::decide_deferred", "sched::schedule_from");
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
             * src/lib/kernel/sched: the direct switch of channel_write_read's fast
             * path (docs/OPAQUE-KERNEL.md §9.7, part 1). */

        requirement <'L.sched.55'> handOverIsTheGeneralSequence : ItemLowLevel {
            attribute :>> statement = "CpuQueue::hand_over shall charge the running
                task and weigh the arriving one as insert does, at one clock reading,
                and RunQueue::hand_over shall leave the queue, the arriving entity
                and the leaving entity's state exactly as enqueue, set_slice_ns,
                remove_curr and pick_next leave them on a queue with one entity
                running and nothing waiting.";
            attribute :>> criterion = "hand_over_is_the_general_sequence: 200,000
                random queues, weights and lags at and past the clamp, virtual times
                either side of the wrap, every field compared; its control changes
                one field of the result and fails.";
            attribute :>> parent = "H.SCHED.13";
            attribute :>> unit = ("sched::queue::CpuQueue::hand_over",
                "sched::queue::CpuQueue::insert_at");
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
                as the shell exits.";
            attribute :>> criterion = "test-ipc-equiv requires trips with the fast
                path on and every count zero with it off.";
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
    }
}
