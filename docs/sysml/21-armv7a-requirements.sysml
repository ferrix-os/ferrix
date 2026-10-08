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
 * §9.14, 3b and F-66). The rest of the
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


    package AddressSpaceIdentifiers {
        doc /* src/kernel/src/arch/armv7a/asid.rs and cpu.rs: ASIDs, one
             * allocator for the machine (docs/OPAQUE-KERNEL.md §9.13). */
        requirement <'L.armv7a.13'> anInstallWritesRootAndNumberAtOnce : ItemLowLevel {
            attribute :>> statement = "install_user_root shall write the root and the
                space's ASID into TTBR0 in one write and synchronize it before it
                clears EPD0, write TTBCR only when EPD0 is set, and make no cache or
                TLB maintenance but a pending flush.";
            attribute :>> criterion = "After an install TTBR0 holds the space's root
                and its number, not 0, bit 0 clear and EPD0 clear (the `asid` line on
                an ARMv7-A boot); the install's instructions are exactly its sequence,
                with no c7 or c8 operation (check-armv7a-asid.py); two spaces on one
                processor each read their own page (the `spaces` line).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::armv7a::install_user_root",
                "arch::armv7a::cpu::install_ttbr0", "arch::armv7a::asid::number_for");
        }
        requirement <'L.armv7a.14'> aProcessorFlushesBeforeANewGeneration : ItemLowLevel {
            attribute :>> statement = "A processor whose flush a rollover left pending
                shall park TTBR0, with EPD0 set and synchronized before TTBR0 becomes
                0, and complete a local TLBIALL, a BPIALL and, where CTR.L1Ip says the
                instruction cache is ASID-tagged, an ICIALLU, before it writes any
                number of the new generation.";
            attribute :>> criterion = "A forced rollover adds a generation and one
                flush on this processor, and every other processor's flush count rises
                at its next install; a space given a reused number reads its own page
                at an address the space before it used (the `asid` line); the flush's
                instructions are exactly its sequence (check-armv7a-asid.py).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::armv7a::cpu::flush_for_new_generation",
                "arch::armv7a::asid::slow_path");
        }
        requirement <'L.armv7a.15'> aKernelThreadRunsWithAsidZero : ItemLowLevel {
            attribute :>> statement = "uninstall_user_root shall set EPD0 and
                synchronize it before it writes TTBR0 to 0, and invalidate nothing.";
            attribute :>> criterion = "After an uninstall TTBR0 reads 0 and EPD0 set
                (the `asid` line); the park's instructions are exactly its sequence
                (check-armv7a-asid.py).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::armv7a::uninstall_user_root",
                "arch::armv7a::cpu::park_ttbr0");
        }
        requirement <'L.armv7a.16'> shootdownsReachEveryProcessor : ItemLowLevel {
            attribute :>> statement = "On ARMv7-A every TLB shootdown shall be broadcast
                to the inner shareable domain and complete before what it covers is
                released.";
            attribute :>> criterion = "The model of the allocator with broadcast
                shootdowns finds no stale hit, and the same model with a shootdown
                scoped to the space's set finds one (ferrix-paging's asid tests).";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("arch::armv7a::flush_tlb_page", "arch::armv7a::flush_tlb");
        }
        requirement <'L.armv7a.17'> userLeavesAreNotGlobal : ItemLowLevel {
            attribute :>> statement = "Every leaf the kernel writes under a user root
                shall be non-global, and once finish_memory has run the kernel's own
                tree shall hold no non-global leaf.";
            attribute :>> criterion = "A user space's leaves are all non-global (the
                `asid` line), and after the loader's alias has gone every kernel leaf
                is global (the `global` line).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::armv7a::drop_identity_map",
                "user::space::AddressSpace::map_anonymous");
        }
        requirement <'L.armv7a.18'> aDroppedSpacesNumberWaitsForAFlush : ItemLowLevel {
            attribute :>> statement = "No processor shall write a dropped space's ASID
                into TTBR0 before its own next full flush.";
            attribute :>> criterion = "The model of the allocator, whose spaces are
                dropped with no shootdown and whose frames and tables are reused at
                once, finds no hit reaching a freed frame or table, and no install
                changing a processor's base while keeping its number
                (ferrix-paging's asid tests).";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = "arch::armv7a::asid::slow_path";
        }
        requirement <'L.armv7a.19'> theFastPathIsOrderedAgainstARollover : ItemLowLevel {
            attribute :>> statement = "number_for shall take a space's number without
                the lock only when the space's tag is of the generation it read and
                swapping the tag into its processor's active tag answers non-zero, and
                a rollover shall swap every processor's active tag with 0, so that no
                processor runs a number in a generation it has not flushed for unless
                it is its own reserved number.";
            attribute :>> criterion = "The loom model of a fast path against another
                processor's rollover finds no two processors running one number for
                two spaces, no stale hit and no unflushed processor running a number
                not reserved for it; its three controls each find one (`cargo xtask
                loom`, src/tests/loom).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::armv7a::asid::number_for",
                "arch::armv7a::asid::slow_path");
        }
        requirement <'L.armv7a.20'> theFlushFollowsTheCore : ItemLowLevel {
            attribute :>> statement = "Each processor shall read its own CTR and ID_MMFR1
                before it installs a space, and add ICIALLU to its flush where CTR.L1Ip
                is 0b01 and BPIALL to every install where ID_MMFR1.BPred is 0b0001.";
            attribute :>> criterion = "The plan is decided as stated for the
                Cortex-A7's values and the two cases that add an operation
                (ferrix-paging's asid tests), and every processor has decided one
                (the `asid` line, which prints the boot processor's CTR and
                ID_MMFR1).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::armv7a::asid::init_this_cpu",
                "arch::armv7a::asid::predictor_every_install");
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
            attribute :>> statement = "save_user_state shall, for a task switched out
                blocked with vectors_dead raised on a core with a VFP, store only
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
                of a task switched out runnable, or blocked in any call but the
                three, and restore_user_state shall restore it whole.";
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
            attribute :>> statement = "restore_user_state shall, for a state marked
                unsaved, load the task's own FPSCR and d8-d15 and zero every other D
                register the core has, and clear the mark; nothing else shall clear
                it but a writer of the whole state.";
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
    }
}
