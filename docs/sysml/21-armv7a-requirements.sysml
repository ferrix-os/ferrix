/*
 * Ferrix — SysML v2 model, part 21: the low-level requirements of
 * arch/armv7a, as far as they are written.
 *
 * ARMv7-A, the Cortex-A7 of the STM32MP157, has no requirements of its own
 * beyond these: what the system call path does for the filter the core asks
 * about every call (seccomp, `docs/SECCOMP.md` §3.3), what its switch
 * barrier does inside a speculation domain (`docs/OPAQUE-KERNEL.md` §9), and
 * what of the generic timer user mode may read (`CNTKCTL`), and what the
 * switch does with the two thread ID registers (`docs/OPAQUE-KERNEL.md`
 * §9.14, 3b and F-66), and how it leaves VFP off for a program that never
 * uses it (§9.15). The rest of the
 * architecture waits in the baseline with the other architectures' unwritten
 * rows. Ids are `L.armv7a.<n>`, flat through the architecture; the rules of
 * part 19 hold.
 */
package FerrixArmv7aRequirements {
    private import FerrixItemRequirements::*;

    package Traps {
        requirement <'L.armv7a.1'> theFilterIsAskedFirstAtTheSvc : ItemLowLevel {
            attribute :>> statement = "system_call shall ask the registered filter
                about a call before set_tls, sigreturn and rt_sigreturn are answered
                and before the dispatcher is, with r7 as the number, the saved pc as
                the instruction after the svc, ARM or Thumb, and ARMv7-A's token, and
                shall apply the filter's answer as the dispatcher's; with nothing
                registered, set_tls and the signal returns shall be answered as they
                were before the filter existed.";
            attribute :>> criterion = "A test-only filter is shown set_tls, sigreturn
                and rt_sigreturn once each, before their answers, with the number, the
                instruction pointer and the first argument the frame held, and its
                errno comes back in r0 (the `seccomp` line).";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = "arch::armv7a::trap::system_call";
        }
        requirement <'L.armv7a.2'> aRolledBackFrameReadsAsAtTheCall : ItemLowLevel {
            attribute :>> statement = "audit_arch shall give the token of the entry a
                call was made through, AUDIT_ARCH_ARM for the svc, and
                syscall_rollback_value shall give r0 the call's first argument so that
                a call that did not run reads as it did at the call, r7 still holding
                the number.";
            attribute :>> criterion = "The value for first argument 0x1111 is 0x1111
                and the token read back from a driven svc is ARMv7-A's, 0x40000028
                (the `seccomp` line).";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = ("arch::armv7a::audit_arch",
                "arch::armv7a::syscall_rollback_value");
        }
    }

    package Speculation {
        requirement <'L.armv7a.3'> nothingInsideADomain : ItemLowLevel {
            attribute :>> statement = "switch_barrier_in_domain shall issue nothing:
                this architecture's switch barrier is BPIALL, or ICIALLU with
                ACTLR.IBE, the predictor invalidation a speculation domain leaves
                out, and no defence of the kernel is on the switch path.";
            attribute :>> criterion = "Switches between two members of one marked
                job decide no barrier and switches out of the domain decide one
                each way (the `domain` line on an ARMv7-A boot).";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = "arch::armv7a::speculation::switch_barrier_in_domain";
        }
    }

    package Timer {
        requirement <'L.armv7a.4'> programsReadTheCounter : ItemLowLevel {
            attribute :>> statement = "allow_user_counter shall set CNTKCTL.PL0VCTEN
                and clear PL0PCTEN, PL0VTEN and PL0PTEN on every processor the kernel
                brings online.";
            attribute :>> criterion = "After every processor is online, each reads
                its own CNTKCTL and finds PL0VCTEN set and the three others clear,
                and the virtual counter advances (the `usercnt` line on an ARMv7-A
                boot, N of N processors); the block driver times its requests with
                CNTVCT read in user mode (the `seam` line's driver share).";
            attribute :>> parent = "H.MEM.2";
            attribute :>> unit = ("arch::armv7a::cpu::allow_user_counter",
                "arch::armv7a::cpu::read_cntkctl");
        }
    }

    package Switch {
        doc /* src/kernel/src/arch/armv7a/switch.rs and trap.rs: the user state
             * the switch moves that no trap saves (docs/OPAQUE-KERNEL.md §9.14;
             * the consultant's verdict, ledger 2026-10-08 line 582, R1-R9). */

        requirement <'L.armv7a.5'> theSvcMarksTheThreeBlockingCalls : ItemLowLevel {
            attribute :>> statement = "system_call shall raise the running task's
                vectors_dead mark before it serves channel_write_read,
                object_wait_one or port_wait with interrupts open, and lower it
                once they are masked again, before the outcome is written and the
                way out; no other number shall raise it.";
            attribute :>> criterion = "Exactly the three numbers of the native range,
                of Linux's first 1,024 and of the Arm private range raise the mark;
                a program woken from channel_write_read keeps its VFP registers
                across a nanosleep after it (the `vectors` line); with the lowering
                removed the check fails by name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::armv7a::trap::dispatch_marked",
                "arch::armv7a::trap::vectors_die_in", "arch::armv7a::trap::mark_vectors_dead");
        }
        requirement <'L.armv7a.6'> aBlockedNativeCallKeepsItsCalleeSaved : ItemLowLevel {
            attribute :>> statement = "save_user_state shall, for a task that has
                used VFP switched out blocked with vectors_dead raised on a core
                with a VFP, store only
                FPSCR and d8-d15 and mark the state unsaved.";
            attribute :>> criterion = "A program blocked in channel_write_read or
                object_wait_one, with every D register its pattern while a second
                program on its core fills them with another, reads back its own
                FPSCR and d8-d15 after a message, its peer's close and a message with
                a signal pending (the `vectors` line); with d8-d15 not kept the check
                fails by name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::armv7a::switch::save_user_state",
                "arch::armv7a::switch::UserState::mark_unsaved");
        }
        requirement <'L.armv7a.7'> aRunnableTaskIsSavedWhole : ItemLowLevel {
            attribute :>> statement = "save_user_state shall save the whole VFP state
                of a task that has used VFP switched out runnable, or blocked in any
                call but the three, and restore_user_state shall restore it
                whole.";
            attribute :>> criterion = "A program blocked in a Linux nanosleep, and one
                preempted in user mode, beside a second program filling every D
                register, read back their own pattern and FPSCR (the `vectors`
                line); with every blocked task treated as unsaved the nanosleep case
                reads them reset and the check fails by name.";
            attribute :>> parent = ("H.SCHED.8", "H.SCHED.12");
            attribute :>> unit = ("arch::armv7a::switch::save_user_state",
                "arch::armv7a::switch::restore_user_state");
        }
        requirement <'L.armv7a.8'> anUnsavedStateIsReset : ItemLowLevel {
            attribute :>> statement = "restore_user_state shall, for a task that has
                used VFP whose state is marked unsaved, load the task's own FPSCR
                and d8-d15 and zero every other D register the core has, and clear
                the mark; nothing else shall clear it but a writer of the whole
                state.";
            attribute :>> criterion = "The wakes of the `vectors` line read zero in
                d0-d7 and d16-d31 and their own d8-d15 and FPSCR, never either
                program's pattern elsewhere; with the zeroing of d0-d7 or of d16-d31
                removed, or the FPSCR load removed, the check fails by name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = "arch::armv7a::switch::restore_user_state";
        }
        requirement <'L.armv7a.9'> anUnsavedStateReadsAsReset : ItemLowLevel {
            attribute :>> statement = "Every reader of a task's saved VFP state shall
                read it through UserState::fp, which shall answer an unsaved state as
                its FPSCR, its own d8-d15 and zero elsewhere; a writer shall leave the
                state whole and not unsaved.";
            attribute :>> criterion = "A state whose doubles hold other bytes, marked
                unsaved, reads through fp as its FPSCR and own d8-d15 and zeros, and
                after set_fp is whole and not unsaved (the `vectors` line); with fp
                answering the raw doubles, or set_fp leaving the mark, the check
                fails by name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::armv7a::switch::UserState::fp",
                "arch::armv7a::switch::UserState::set_fp", "arch::armv7a::switch::UserState::is_unsaved");
        }
        requirement <'L.armv7a.10'> theThreadPointerIsTheRecords : ItemLowLevel {
            attribute :>> statement = "set_tls shall write TPIDRURO and the running
                task's record together with interrupts masked, execve's
                reset_user_state shall zero both in one window with interrupts
                masked, save_user_state shall not read TPIDRURO, and
                restore_user_state shall write the incoming task's recorded
                TPIDRURO at every switch to a task with user state, never skipping
                a write equal to what the processor last held.";
            attribute :>> criterion = "A program reads its own thread pointer after
                a nanosleep; two programs with different thread pointers trade one
                core 1,000 times each, each reading its own; an execve'd image,
                switched out and in, reads 0 (the `tls` line); with set_tls writing
                no record, the restore's write removed, or execve leaving the record,
                the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::armv7a::switch::save_user_state",
                "arch::armv7a::switch::restore_user_state",
                "arch::armv7a::switch::reset_user_state", "arch::armv7a::trap::answer_set_tls");
        }
        requirement <'L.armv7a.11'> theUserRegisterIsSwitched : ItemLowLevel {
            attribute :>> statement = "save_user_state shall read TPIDRURW at every
                switch out, blocked or not, and restore_user_state shall write the
                incoming task's at every switch to a task with user state, never
                skipping a write by comparing with any remembered value.";
            attribute :>> criterion = "Two programs with different TPIDRURW trade one
                core 1,000 times each, each reading its own; a program whose value
                equals the one its core last had written reads its own beside one
                that changed the register after that write (the `tls` line); with
                the restore's write removed, as before F-66's fix, or skipped where
                it equals the last one written, the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::armv7a::switch::save_user_state",
                "arch::armv7a::switch::restore_user_state", "arch::armv7a::cpu::read_tpidrurw",
                "arch::armv7a::cpu::write_tpidrurw");
        }
        requirement <'L.armv7a.12'> theUserRegisterIsInheritedAndReset : ItemLowLevel {
            attribute :>> statement = "UserState::capture shall carry the caller's
                TPIDRURW to a fork child or a new thread, and execve's
                reset_user_state shall zero it.";
            attribute :>> criterion = "A fork child, switched out and in, reads its
                parent's TPIDRURW, and an execve'd image reads 0 (the `tls` line);
                with capture leaving it out, or execve leaving it, the check fails
                by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::armv7a::switch::UserState::capture",
                "arch::armv7a::switch::reset_user_state");
        }
        requirement <'L.armv7a.21'> aTaskWithoutVfpMovesNothing : ItemLowLevel {
            attribute :>> statement = "A task that has not used VFP shall run with
                FPEXC.EN clear; save_user_state shall move no VFP register for it,
                and restore_user_state, switching to it on a core whose EN is clear,
                shall execute no VFP instruction and write no FPEXC; each core's
                record of EN shall equal the register.";
            attribute :>> criterion = "Two native programs that never touch VFP make
                1,000 channel_write_read round trips pinned to one core, and two
                more trade it by sched_yield: each task's count of VFP moves and
                FPEXC writes is 0, and the record equals FPEXC (the `lazy` line);
                with a switch to a task that has not used VFP turning VFP on and
                zeroing the registers every time the check fails by name.";
            attribute :>> parent = ("H.SCHED.8", "H.SCHED.12");
            attribute :>> unit = ("arch::armv7a::switch::save_user_state",
                "arch::armv7a::switch::restore_user_state", "arch::armv7a::switch::vfp_on");
        }
        requirement <'L.armv7a.22'> theFirstUseIsTaken : ItemLowLevel {
            attribute :>> statement = "An undefined instruction from user mode, on a
                core with a VFP, by a task with user state that has not used VFP,
                shall be taken with interrupts masked as its first use: the task
                marked, FPEXC.EN set with an isb, the record set, the task's own
                record loaded through the switch's own load, and the instruction
                re-executed at its own address in ARM and Thumb state and inside an
                IT block; a record that disagrees with the task's mark shall be a
                kernel fault report.";
            attribute :>> criterion = "Programs whose first VFP instruction is in ARM
                state, in Thumb state after a 16-bit increment, and inside an IT
                block, each run after a program that filled every register, read
                zero in FPSCR and every D register and compute their sum, and a PL0
                read of FPEXC, FPSID, MVFR0 or MVFR1 after the first use ends in
                SIGILL (the `lazy` line); with the first use not taken in Thumb
                state, the Thumb retry two bytes early, or the IT state cleared,
                the check fails by name (with no first use at all, stage 3's
                signal check, whose program uses VFP, fails first).";
            attribute :>> parent = ("H.SCHED.8", "H.TRAP.4");
            attribute :>> unit = ("arch::armv7a::switch::take_first_use",
                "arch::armv7a::trap::ferrix_trap_entry");
        }
        requirement <'L.armv7a.23'> sigillNamesItsInstruction : ItemLowLevel {
            attribute :>> statement = "The undefined-instruction path shall set the
                frame's pc to the instruction's own address -- LR_und less 4 in ARM
                state and less 2 in Thumb state -- once, before both the first use
                and the SIGILL, so that a SIGILL's si_addr and the signal frame's pc
                name the instruction.";
            attribute :>> criterion = "An ARM udf, a 16-bit Thumb udf and a 32-bit
                Thumb udf.w, in a program that has used VFP, each end in a SIGILL
                whose handler reads si_addr and the ucontext pc equal to the
                instruction's address (the `lazy` line); with the Thumb correction
                removed, F-68's stub, the check fails by name.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::armv7a::trap::point_at_undefined",
                "arch::armv7a::trap::ferrix_trap_entry");
        }
        requirement <'L.armv7a.24'> nothingStaysBehindAClearEnable : ItemLowLevel {
            attribute :>> statement = "While a core's FPEXC.EN is clear, every VFP
                register it has and FPSCR shall be zero: bring-up, a switch to a
                task that has not used VFP from a core whose EN is set, and execve
                on such a core shall zero them before clearing EN.";
            attribute :>> criterion = "Bring-up's VFP sequence run over another
                program's values, a program that never uses VFP switched in after
                one that filled every register, and an image a program with VFP
                state execve'd leave EN clear and every register and FPSCR zero, as
                the kernel reads them with interrupts masked (the `lazy` line);
                with any of the three zeroings removed the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::armv7a::switch::bring_up_vfp",
                "arch::armv7a::switch::restore_user_state",
                "arch::armv7a::switch::reset_user_state", "arch::armv7a::switch::scrub_off");
        }
        requirement <'L.armv7a.25'> theRecordIsTheStateWhileOff : ItemLowLevel {
            attribute :>> statement = "While a task has not used VFP its record shall
                be its VFP state: UserState::capture shall copy it, and its mark,
                for a signal frame, a fork child or a thread, sigreturn shall write
                it, and execve shall clear it with the mark, each deciding by the
                running task's mark read with interrupts masked and executing no
                VFP instruction with EN clear; for a task that has used VFP they
                shall read and write the registers.";
            attribute :>> criterion = "A program that never used VFP gets a state
                into its record by a signal frame and reads it at its first use; a
                second frame and a fork child's first use carry it; a fork child of
                a program with VFP state reads its parent's registers; an image
                execve'd by a program whose record held a state reads zero at its
                first use (the `lazy` line); with sigreturn writing no record,
                capture answering the initial state, a fork child of a program
                with VFP state loaded with zeros at its first switch in, or execve
                leaving the record, the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::armv7a::switch::UserState::capture",
                "arch::armv7a::switch::load_user_fp",
                "arch::armv7a::switch::reset_user_state");
        }
        requirement <'L.armv7a.26'> aVfpUserIsResumedWhole : ItemLowLevel {
            attribute :>> statement = "A task that has used VFP shall run with
                FPEXC.EN set and its own state on whichever core resumes it: its
                state saved at every switch out, so no core holds a state another
                must fetch.";
            attribute :>> criterion = "A program with VFP state, unpinned, naps 20
                times while a program pinned to the core it slept on keeps it busy,
                runs on two cores, and reads its whole state after every nap (the
                `lazy` line at --smp 2); the case says it did not run unless the
                program ran on two cores.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::armv7a::switch::save_user_state",
                "arch::armv7a::switch::restore_user_state",
                "arch::armv7a::switch::turn_on");
        }
    }
}
