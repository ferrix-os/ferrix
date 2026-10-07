/*
 * Ferrix — SysML v2 model, part 21: the low-level requirements of
 * arch/armv7a, as far as they are written.
 *
 * ARMv7-A, the Cortex-A7 of the STM32MP157, has no requirements of its own
 * beyond these: what the system call path does for the filter the core asks
 * about every call (seccomp, `docs/SECCOMP.md` §3.3), what its switch
 * barrier does inside a speculation domain (`docs/OPAQUE-KERNEL.md` §9), and
 * what of the generic timer user mode may read (`CNTKCTL`). The rest of the
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
}
