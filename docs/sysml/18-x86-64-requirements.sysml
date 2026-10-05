/*
 * Ferrix — SysML v2 model, part 18: the low-level requirements of the x86-64
 * architecture, the trap path and the system call entry.
 *
 * What each unit of src/kernel/src/arch/x86_64/, src/kernel/src/trap.rs and
 * src/kernel/src/syscall/mod.rs's dispatcher does, as `ItemLowLevel` requirements
 * (part 13 defines the format): the descriptor tables and which selectors ring 3
 * may hold, the context switch and the user state it carries, starting
 * processors, the paranoid entries, the speculation defences, the counter and
 * timer, both ABIs' signal frames, SYSCALL and int $0x80, the exception gates and
 * what a fault becomes, and which calls reach which answer.
 *
 * The format is the object/ pilot's (W-8 step 3), and so are its rules: a
 * requirement names every function that carries its behaviour as its `unit`;
 * one check proves a whole requirement, so a requirement is split where the
 * checks that exist each prove a part; a statement says no more than its
 * criterion tests; an accessor needs no requirement of its own.
 *
 * What proves what. Most of this is proved by programs the boot runs in ring 3:
 * arch/x86_64/trap/check.rs's faulting programs and its i386 programs (int $0x80,
 * thread-local segments, both signal frames and a fork, forged frames,
 * socketcall), syscall/check.rs's 64-bit ones, and paranoid/check.rs's
 * breakpoints in the SYSCALL window. Two limits of the default gate are said
 * where they matter: under TCG the processor offers no speculation controls and
 * the counter is the HPET, so the read-back and the TSC paths are exercised by
 * the KVM boot row only; and several checks the boot runs -- stage 3's
 * breakpoints and demand paging, the timer's rate, CR0.WP -- are functions of
 * main.rs, product code a tag cannot go on, so their requirements wait in the
 * baseline until the checks move. So does every criterion no check tests:
 * docs/certification/IMPLEMENTATION.md W-8 lists them.
 *
 * Ids are `L.x86_64.<n>`, `L.trap.<n>` and `L.syscall.<n>`, numbered through
 * each; the package gives the part.
 */
package FerrixX8664Requirements {
    private import FerrixItemRequirements::*;

    package Descriptors {
        doc /* src/kernel/src/arch/x86_64/gdt.rs: each processor's descriptor tables, its
             * task state, and which selectors ring 3 may hold. */

        requirement <'L.x86_64.1'> selectorsRing3MayHoldAreLoadable : ItemLowLevel {
            attribute :>> statement = "loadable shall answer a selector, with RPL
                forced to 3, when it names 32-bit user code, user data, or a
                thread-local slot whose descriptor ring_3_segment accepts --
                present, code or data, DPL 3, no L bit, and data or readable
                non-conforming code -- and the null selector for every other, one
                with the LDT bit included.";
            attribute :>> criterion = "7 selectors -- user data and a thread-local
                data slot at RPL 3 and at RPL 0, 32-bit user code, and null at RPL 0
                and 3 -- load at RPL 3 or as null; 18 others -- kernel code and
                data, 64-bit user code, the TSS, slots 1, 10 and 15, an LDT
                selector, 0xFFFF, and a thread-local slot holding nothing, DPL-0
                data, a call gate, a TSS, an L-bit segment, an absent segment,
                unreadable code or conforming code -- load as null (the `gdt` line).";
            attribute :>> parent = "H.TRAP.7";
            attribute :>> unit = ("arch::x86_64::gdt::loadable",
                "arch::x86_64::gdt::ring_3_segment");
        }
        requirement <'L.x86_64.2'> compatUserSegmentsInGdt : ItemLowLevel {
            attribute :>> statement = "load, for init and init_secondary, shall put
                a flat 4 GiB ring-3 32-bit code segment at selector 0x23 and ring-3
                data at 0x2b in the processor's GDT, so that a program runs in
                compatibility mode there.";
            attribute :>> criterion = "An i386 program entered at 0x23 with its
                stack at 0x2b writes a line through int $0x80 and exits 42 (the
                first `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::gdt::load",
                "arch::x86_64::gdt::init",
                "arch::x86_64::gdt::init_secondary");
        }
        requirement <'L.x86_64.3'> istStacksPerProcessor : ItemLowLevel {
            attribute :>> statement = "init and init_secondary shall load on each
                processor its own TSS, through the descriptor tss_descriptor
                encodes, whose interrupt stack table names that processor's own
                double-fault, NMI, debug and machine-check stacks with the occupancy
                word at each zero, and whose RSP0 is nonzero.";
            attribute :>> criterion = "On every processor, the TSS the task register
                names has that processor's table as its base and limit 103; its four
                IST entries lie in four stacks no other processor's TSS names, each
                occupancy word 0; RSP0 is nonzero.";
            attribute :>> parent = ("H.FAIL.2", "H.TRAP.6");
            attribute :>> unit = ("arch::x86_64::gdt::init",
                "arch::x86_64::gdt::init_secondary",
                "arch::x86_64::gdt::load",
                "arch::x86_64::gdt::tss_descriptor",
                "arch::x86_64::gdt::TaskStateSegment::set_interrupt_stack",
                "arch::x86_64::gdt::TaskStateSegment::write_u64");
        }
        requirement <'L.x86_64.4'> ringThreeDeniedEveryPort : ItemLowLevel {
            attribute :>> statement = "load shall set every TSS's I/O permission
                bitmap offset past its end, so that ring 3 may touch no port.";
            attribute :>> criterion = "A program executing inb and outb on a port is
                ended by SIGSEGV, 2 of 2.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::x86_64::gdt::load",
                "arch::x86_64::gdt::TaskStateSegment::write_u16");
        }
        requirement <'L.x86_64.5'> secondaryGdtFailureIsFatal : ItemLowLevel {
            attribute :>> statement = "init_secondary shall report a failed
                interrupt-stack or table allocation, and secondary_start shall end
                the boot in the catalogued SECONDARY_GDT report.";
            attribute :>> criterion = "A secondary start whose stack allocation
                fails ends in the SECONDARY_GDT report, and no FERRIX-BOOT-OK.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::gdt::init_secondary",
                "arch::x86_64::smp::secondary_start");
        }
        requirement <'L.x86_64.6'> entryStackFollowsTask : ItemLowLevel {
            attribute :>> statement = "restore_user_state shall point this
                processor's TSS RSP0, found through STR and SGDT by
                set_privilege_stack, and its SYSCALL entry stack at the incoming
                task's kernel-stack top.";
            attribute :>> criterion = "After each of N switches between two user
                tasks on one processor, RSP0 and the per-processor entry stack read
                back equal to the running task's kernel-stack top: 0 mismatches.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::gdt::set_privilege_stack",
                "arch::x86_64::switch::restore_user_state");
        }
    }

    package UserState {
        doc /* src/kernel/src/arch/x86_64/switch.rs, with gdt.rs's thread-local slots: the
             * context switch, and the user-mode state a trap does not save. */

        requirement <'L.x86_64.7'> threadAreaInLiveGdt : ItemLowLevel {
            attribute :>> statement = "set_thread_area shall write a descriptor into
                the calling thread's slot of this processor's live GDT, and
                thread_area shall read the slot back, each with interrupts masked.";
            attribute :>> criterion = "An i386 program's set_thread_area with entry
                -1 is given 12, then 13, then 14; %gs loaded with 0x63 reads its own
                word; get_thread_area(12) answers its base; lar on 0x73 from ring 3
                finds DPL 3 and no L bit; the program exits 42 (the thread-local
                `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::switch::set_thread_area",
                "arch::x86_64::switch::thread_area",
                "arch::x86_64::switch::with_interrupts_masked",
                "arch::x86_64::gdt::write_tls",
                "arch::x86_64::gdt::read_tls",
                "arch::x86_64::gdt::live_table");
        }
        requirement <'L.x86_64.8'> threadAreaTravelsWithThread : ItemLowLevel {
            attribute :>> statement = "save_user_state shall save the outgoing
                thread's thread-local slots and data selectors, and
                restore_user_state shall write the incoming thread's slots and
                reload its selectors through loadable.";
            attribute :>> criterion = "Two i386 programs pinned to one processor,
                both with entry 12 and %gs 0x63 but bases 4 bytes apart, each read
                their own word through %gs 200 times with a sched_yield between
                reads: neither exits 46, both exit 42.";
            attribute :>> parent = "H.SCHED.7";
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::cpu::read_data_selectors",
                "arch::x86_64::cpu::load_user_gs");
        }
        requirement <'L.x86_64.9'> fsGsBaseTravelsWithTask : ItemLowLevel {
            attribute :>> statement = "A task's FS and GS bases shall be its
                UserState's record, written by arch_prctl and execve beside the MSRs
                and never read back at a switch, and restore_user_state shall load
                each where the thread's FS or GS selector is null.";
            attribute :>> criterion = "Two 64-bit programs pinned to one processor,
                each with its own ARCH_SET_FS and ARCH_SET_GS base, read their own
                words through %fs and %gs N times across sched_yield: 0 foreign
                reads.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::UserState::set_bases");
        }
        requirement <'L.x86_64.10'> fpuStateTravelsWithTask : ItemLowLevel {
            attribute :>> statement = "save_user_state shall save the outgoing
                task's x87 and SSE state, and its AVX state where
                enable_extended_state enabled it, with XSAVE64 of the enabled
                components, or FXSAVE64 without XSAVE; restore_user_state shall
                restore the incoming task's with XRSTOR64, or FXRSTOR64; except for a
                task blocked in a call of the vector-state contract (L.x86_64.153,
                L.x86_64.155).";
            attribute :>> criterion = "Two programs pinned to one processor, each
                holding distinct XMM values, YMM upper halves where AVX is enabled,
                and MXCSR and x87 control words across N sched_yields, read back
                their own: 0 mismatches.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::ferrix_fpu_save",
                "arch::x86_64::switch::ferrix_fpu_restore");
        }
        requirement <'L.x86_64.11'> forkChildInheritsUserState : ItemLowLevel {
            attribute :>> statement = "UserState::capture shall copy the calling
                task's FS and GS bases, thread-local slots, data selectors and x87
                and SSE state for a fork child.";
            attribute :>> criterion = "A fork child reads its parent's %fs word,
                MXCSR and an XMM value set before the fork, and an i386 child its
                parent's %gs word.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = "arch::x86_64::switch::UserState::capture";
        }
        requirement <'L.x86_64.12'> newProgramStartsWithResetState : ItemLowLevel {
            attribute :>> statement = "reset_user_state shall, at execve, empty the
                thread-local slots, load null data selectors with zero FS and GS
                bases, and load UserState::new's x87 control word 0x037F and MXCSR
                0x1F80.";
            attribute :>> criterion = "A program that set a thread pointer, a
                thread-local entry and an unmasked MXCSR and then execve's finds in
                the new image FS base 0, get_thread_area(12) empty, MXCSR 0x1F80 and
                x87 control word 0x037F.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::switch::reset_user_state",
                "arch::x86_64::switch::UserState::new");
        }
        requirement <'L.x86_64.152'> theEntryMarksTheThreeBlockingCalls : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry shall raise the running
                task's vectors_dead mark before it serves channel_write_read,
                object_wait_one or port_wait, and lower it as the call returns,
                before the way out; no other number shall raise it.";
            attribute :>> criterion = "Exactly the three numbers of the native range
                and of Linux's first 1,024 raise the mark (the `vectors` line); a
                program woken from channel_write_read keeps its vector registers
                across a nanosleep after it, and with the lowering removed it reads
                them reset and the check fails by name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::x86_64::syscall::ferrix_syscall_entry",
                "arch::x86_64::syscall::vectors_die_in",
                "arch::x86_64::syscall::mark_vectors_dead");
        }
        requirement <'L.x86_64.153'> aBlockedNativeCallKeepsOnlyItsControls : ItemLowLevel {
            attribute :>> statement = "save_user_state shall, for a task switched out
                blocked with vectors_dead raised on a processor that saves with
                XSAVE, store only MXCSR and the x87 control word, in the standard
                form's legacy region with XSTATE_BV empty, and mark the state
                unsaved.";
            attribute :>> criterion = "A program blocked in channel_write_read or
                object_wait_one, with every vector register its pattern while a
                second program on its processor fills them with another, reads back
                the initial state with its own MXCSR and control word after a
                message, its peer's close and a message with a signal pending (the
                `vectors` line).";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::UserState::keep_vector_controls",
                "arch::x86_64::cpu::read_vector_controls");
        }
        requirement <'L.x86_64.154'> aRunnableTaskIsSavedWhole : ItemLowLevel {
            attribute :>> statement = "save_user_state shall save the whole vector
                state of a task switched out runnable, or blocked in any call but the
                three, and restore_user_state shall restore it whole.";
            attribute :>> criterion = "A program blocked in a Linux nanosleep, and
                one preempted in user mode, beside a second program filling every
                vector register, read back their own pattern, MXCSR and control word
                (the `vectors` line); with every blocked task treated as unsaved the
                nanosleep case reads them reset and the check fails by name.";
            attribute :>> parent = ("H.SCHED.8", "H.SCHED.12");
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::restore_user_state");
        }
        requirement <'L.x86_64.155'> anUnsavedStateIsReset : ItemLowLevel {
            attribute :>> statement = "restore_user_state shall, for a state marked
                unsaved, XRSTOR64 the area with an empty XSTATE_BV and XCR0's enabled
                components less PKRU requested, which initialises every one and loads
                the task's own MXCSR, then load the task's x87 control word where it
                is not 0x037F, and clear the mark; nothing else shall clear it.";
            attribute :>> criterion = "The three wakes of the `vectors` line read
                neither program's pattern; with the reset skipped and the mark
                cleared the first reads the second program's and the check fails by
                name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::reset_vectors", "arch::x86_64::cpu::load_x87_control");
        }
        requirement <'L.x86_64.156'> anUnsavedStateReadsAsInitial : ItemLowLevel {
            attribute :>> statement = "Every reader of a task's saved vector area
                shall read it through UserState::vectors, which shall answer an
                unsaved state as the initial state with the task's own MXCSR and x87
                control word; a writer shall first make it that state.";
            attribute :>> criterion = "A state whose area holds other bytes, kept
                with keep_vector_controls, reads as FpuArea::initial with its own
                MXCSR and control word through fxsave, avx and xstate_bv, and still
                does after a writer takes it (the `vectors` line).";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::x86_64::switch::UserState::vectors",
                "arch::x86_64::switch::UserState::fxsave", "arch::x86_64::switch::UserState::avx",
                "arch::x86_64::switch::UserState::xstate_bv",
                "arch::x86_64::switch::UserState::fxsave_mut",
                "arch::x86_64::switch::UserState::avx_mut",
                "arch::x86_64::switch::UserState::materialise",
                "arch::x86_64::switch::FpuArea::initial");
        }
        requirement <'L.x86_64.157'> pkruIsNeverReset : ItemLowLevel {
            attribute :>> statement = "The vector reset's requested components shall
                never include PKRU, and no state component past x87, SSE and AVX
                shall be enabled for it.";
            attribute :>> criterion = "reset_components holds no PKRU bit and the
                enabled components are within x87, SSE and AVX, at every boot (the
                `vectors` line).";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::x86_64::cpu::reset_components",
                "arch::x86_64::cpu::enable_extended_state");
        }
        requirement <'L.x86_64.158'> theBasesAreWrittenAtEverySwitch : ItemLowLevel {
            attribute :>> statement = "restore_user_state shall write the incoming
                task's recorded FS and GS bases where its selectors are null at every
                switch to a task with user state, never skipping a write equal to
                what the processor last held, and save_user_state shall read no base
                MSR.";
            attribute :>> criterion = "Two programs with different FS bases trade one
                processor 10,000 times each, each reading its own word (the `fsbase`
                line); a program that loads USER_DS and then a null selector into FS
                before each yield leaves the base its processor last wrote, and a
                second whose recorded base equals it reads its own word at every
                turn; with the write skipped where it equals the last one written,
                the second ends with SIGSEGV and the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::save_user_state");
        }
        requirement <'L.x86_64.13'> sigreturnLoadsFpu : ItemLowLevel {
            attribute :>> statement = "load_fpu shall load the x87 and SSE registers
                from the state rt_sigreturn filled from the frame.";
            attribute :>> criterion = "A handler that changes its frame's fpstate
                MXCSR (legal bits) and an XMM register returns to find both changed.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = "arch::x86_64::switch::load_fpu";
        }
        requirement <'L.x86_64.14'> compatEntryHasUserData : ItemLowLevel {
            attribute :>> statement = "enter_compat_segments shall load user data
                (0x2b) into DS and ES and null into FS as a 32-bit program is first
                entered or execve'd.";
            attribute :>> criterion = "An i386 program whose first instructions read
                and write through DS and ES exits 42, on first entry and after an
                execve.";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = "arch::x86_64::switch::enter_compat_segments";
        }
        requirement <'L.x86_64.15'> handlerEnteredWithUserData : ItemLowLevel {
            attribute :>> statement = "A 32-bit program's signal handler shall be
                entered with user data in DS and ES, loaded through loadable against
                the running thread's slots with interrupts masked.";
            attribute :>> criterion = "The i386 signal program's SA_SIGINFO handler
                reads its siginfo and writes its ucontext's EAX through DS, the
                plain handler writes its sigcontext, both return, and the program
                exits 42 (the signal `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::switch::load_program_selectors",
                "arch::x86_64::cpu::load_data_selectors");
        }
        requirement <'L.x86_64.16'> staleSelectorLoadsNull : ItemLowLevel {
            attribute :>> statement = "restore_user_state and load_program_selectors
                shall load each data selector only through loadable, so that a saved
                selector naming a slot emptied since loads as null instead of
                faulting in ring 0.";
            attribute :>> criterion = "A 32-bit thread whose saved %fs names a
                thread-local slot emptied before it is next restored resumes with
                %fs 0, and the boot continues.";
            attribute :>> parent = "H.TRAP.7";
            attribute :>> unit = ("arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::load_program_selectors");
        }
        requirement <'L.x86_64.17'> contextSwitchStartsAndResumes : ItemLowLevel {
            attribute :>> statement = "prepare_stack shall lay out a stack so that
                the first switch_to it runs the task's entry, and switch_to shall
                save one context's callee-saved registers and stack pointer and
                resume another's.";
            attribute :>> criterion = "1000 threads spawned on one processor all run
                to completion, each switched to at least once (the `tasks` line).";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("arch::x86_64::switch::switch_to",
                "arch::x86_64::switch::prepare_stack");
        }
    }

    package Processors {
        doc /* src/kernel/src/arch/x86_64/smp.rs and apic.rs's commands: starting each
             * processor, and the interrupts processors send each other. */

        requirement <'L.x86_64.18'> everyListedProcessorStarts : ItemLowLevel {
            attribute :>> statement = "describe_cpus shall list the processors the
                MADT enables, and CpuStarter, send and issue shall start each with
                INIT and start-up commands to the APIC ID it names, so that it runs
                kernel work under its own per-processor record.";
            attribute :>> criterion = "Each of the N processors listed runs its
                share of the work in each of 100 rounds, 0 of it on a processor
                whose APIC ID is not its record's (the `smp` line).";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("arch::x86_64::smp::describe_cpus",
                "arch::x86_64::smp::CpuStarter::new",
                "arch::x86_64::smp::CpuStarter::start",
                "arch::x86_64::smp::spin_nanos",
                "arch::x86_64::smp::secondary_start",
                "arch::x86_64::apic::send",
                "arch::x86_64::apic::issue",
                "arch::x86_64::apic::lapic");
        }
        requirement <'L.x86_64.19'> broadcastIpiReachesEverySecondary : ItemLowLevel {
            attribute :>> statement = "issue with the all-but-self shorthand shall
                deliver a fixed interrupt on the IPI vector to every other started
                processor, whose local APIC init_this_cpu enabled at task priority
                0.";
            attribute :>> criterion = "Over 100 rounds of work run everywhere, every
                secondary takes at least one inter-processor interrupt (the `smp`
                line).";
            attribute :>> parent = ("H.MEM.7", "H.SCHED.1");
            attribute :>> unit = ("arch::x86_64::apic::issue",
                "arch::x86_64::apic::init_this_cpu");
        }
        requirement <'L.x86_64.20'> trampolineFramesReturned : ItemLowLevel {
            attribute :>> statement = "CpuStarter::finish shall unmap the start-up
                trampoline's identity mapping and give its code page and root table
                back to the frame allocator.";
            attribute :>> criterion = "The free frame count after finish equals the
                count before CpuStarter::new, and the trampoline's page does not
                translate in the root.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "arch::x86_64::smp::CpuStarter::finish";
        }
        requirement <'L.x86_64.21'> secondaryAdoptsBootCr4 : ItemLowLevel {
            attribute :>> statement = "CpuStarter::new shall take the boot
                processor's CR4, and secondary_start shall load it on each secondary
                once it is in long mode.";
            attribute :>> criterion = "Each of the N online processors reads CR4
                equal to the boot processor's, SMEP and SMAP set where offered.";
            attribute :>> parent = "H.MEM.3";
            attribute :>> unit = ("arch::x86_64::smp::CpuStarter::new",
                "arch::x86_64::smp::secondary_start",
                "arch::x86_64::cpu::read_cr4",
                "arch::x86_64::cpu::write_cr4",
                "arch::x86_64::cpu::read_cr0",
                "arch::x86_64::cpu::read_msr",
                "arch::x86_64::cpu::write_cr3");
        }
        requirement <'L.x86_64.22'> directedIpiReachesOnlyItsTarget : ItemLowLevel {
            attribute :>> statement = "send shall deliver a fixed interrupt on the
                IPI vector to the one processor whose APIC ID it names.";
            attribute :>> criterion = "For each secondary, send_ipi_to raises that
                processor's IPI count by at least 1 and leaves every other
                secondary's unchanged.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("arch::x86_64::apic::send", "arch::x86_64::apic::issue");
        }
        requirement <'L.x86_64.23'> wideApicIdRefused : ItemLowLevel {
            attribute :>> statement = "send shall refuse an APIC ID above 255
                without writing the interrupt command register.";
            attribute :>> criterion = "send_ipi_to(256) answers an error and no
                processor's IPI count moves.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = "arch::x86_64::apic::send";
        }
    }

    package ParanoidEntries {
        doc /* src/kernel/src/arch/x86_64/paranoid.rs: the NMI, #DB and #DF entries, which
             * decide GS for themselves. */

        requirement <'L.x86_64.24'> kernelNmiSurvived : ItemLowLevel {
            attribute :>> statement = "ferrix_paranoid_entry shall count an NMI
                taken in the kernel and return from it, with note_gs finding its GS
                naming a processor's record, and send and issue shall deliver the
                NMI a processor sends to its own APIC ID.";
            attribute :>> criterion = "An NMI sent to this processor with interrupts
                masked is accepted, at least one NMI is counted and returned from,
                and 0 handler runs find a GS naming no processor's record (the `nmi`
                line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::paranoid::ferrix_paranoid_entry",
                "arch::x86_64::paranoid::note_gs",
                "arch::x86_64::apic::id");
        }
        requirement <'L.x86_64.25'> syscallWindowBreakpointsSurvived : ItemLowLevel {
            attribute :>> statement = "debug shall count a kernel #DB against the
                breakpoint slot DR6 names, set RF so that the instruction runs, and
                return, with note_gs finding the kernel's GS, including in the
                SYSCALL trampoline's stretches on the program's stack.";
            attribute :>> criterion = "Breakpoints on the SYSCALL stub's swapgs and
                on its sysretq, while a program writes and exits, fire at least 2
                and at least 1 times, 0 of them with a foreign GS, and the program
                exits with its status (the first `debug` line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::paranoid::ferrix_paranoid_entry",
                "arch::x86_64::paranoid::debug",
                "arch::x86_64::paranoid::take_debug_status",
                "arch::x86_64::paranoid::note_gs");
        }
        requirement <'L.x86_64.26'> debugStatusCountedOnce : ItemLowLevel {
            attribute :>> statement = "take_debug_status shall read and clear DR6 on
                each kernel #DB, so that debug counts each breakpoint once and runs
                the hook once per #DB.";
            attribute :>> criterion = "With breakpoints armed on a kernel function
                and on the hook every #DB handler calls, one call of each counts
                exactly 1 hit per slot and 3 hook runs (the second `debug` line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::paranoid::take_debug_status",
                "arch::x86_64::paranoid::debug",
                "arch::x86_64::paranoid::ferrix_paranoid_entry",
                "arch::x86_64::cpu::read_dr6",
                "arch::x86_64::cpu::write_dr6");
        }
        requirement <'L.x86_64.27'> paranoidUnexpectedIsFatal : ItemLowLevel {
            attribute :>> statement = "ferrix_paranoid_entry shall end in its
                catalogued report a second exception on one IST stack, a machine
                check and any other vector, and debug shall end in its report a
                kernel #DB that no breakpoint raised.";
            attribute :>> criterion = "Each of a provoked nested IST entry, a kernel
                single-step #DB and an injected #MC ends in its catalogued report,
                and no FERRIX-BOOT-OK.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::paranoid::ferrix_paranoid_entry",
                "arch::x86_64::paranoid::debug");
        }
    }

    package Speculation {
        doc /* src/kernel/src/arch/x86_64/speculation.rs: the side-channel defences, per
             * processor, and the index clamp. */

        requirement <'L.x86_64.28'> everyProcessorRecordsItsDefences : ItemLowLevel {
            attribute :>> statement = "init on the boot processor and apply_this_cpu
                on each secondary shall record the side-channel defences the
                processor applied: clamped indices among them in a hardened build,
                none in a --mitigations off build.";
            attribute :>> criterion = "Each of the N processors has a record; in a
                hardened boot each includes clamped indices, in a --mitigations off
                boot each is empty (the `cpu` line).";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::init",
                "arch::x86_64::speculation::apply_this_cpu",
                "arch::x86_64::speculation::apply",
                "arch::x86_64::speculation::Plan::for_processor");
        }
        requirement <'L.x86_64.29'> speculationControlsReadBack : ItemLowLevel {
            attribute :>> statement = "apply shall read back each control the plan
                writes -- IA32_SPEC_CTRL's IBRS, STIBP and SSBD, EFER.AIBRSE, AMD's
                VIRT_SPEC_CTRL.SSBD -- and record READ_BACK_FAILED on a processor
                where one did not hold.";
            attribute :>> criterion = "Under KVM, where the processor offers the
                controls (`test-boot --accel kvm`), 0 of the N processors record
                READ_BACK_FAILED. Under TCG the processor offers none and nothing is
                written.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::apply",
                "arch::x86_64::speculation::init",
                "arch::x86_64::speculation::apply_this_cpu",
                "arch::x86_64::cpu::write_msr",
                "arch::x86_64::cpu::read_msr");
        }
        requirement <'L.x86_64.30'> indexClampedUnderSpeculation : ItemLowLevel {
            attribute :>> statement = "clamp_index and clamp_below shall answer the
                index when it is below its bound and 0 when it is not, in a hardened
                build, and the index unchanged in a --mitigations off build.";
            attribute :>> criterion = "Hardened: indices inside their bound come
                back themselves and clamp_index(9, 4) and clamp_below(u64::MAX,
                0x2000) through black_box come back 0; --mitigations off: both
                answer their input.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::clamp_index",
                "arch::x86_64::speculation::clamp_below");
        }
        requirement <'L.x86_64.31'> switchBarrierIssued : ItemLowLevel {
            attribute :>> statement = "switch_barrier shall, in a hardened build,
                refill the return stack with 32 entries and answer that it issued a
                barrier.";
            attribute :>> criterion = "After programs have run in different address
                spaces, a hardened boot counts at least one switch barrier (the
                `cpu` line).";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::switch_barrier",
                "arch::x86_64::speculation::fill_return_stack");
        }
        requirement <'L.x86_64.126'> refillKeptInsideADomain : ItemLowLevel {
            attribute :>> statement = "switch_barrier_in_domain shall, in a hardened
                build, refill the return stack with 32 entries and issue no IBPB:
                inside one speculation domain only the predictor invalidation is
                left out.";
            attribute :>> criterion = "Switches between two members of one marked
                job decide no barrier (the `domain` line, case 1), and the hardened
                build's return-stack refill runs at each of them.";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = ("arch::x86_64::speculation::switch_barrier_in_domain",
                "arch::x86_64::speculation::fill_return_stack");
        }
        requirement <'L.x86_64.127'> patEntryOneWriteCombining : ItemLowLevel {
            attribute :>> statement = "Every processor shall program IA32_PAT with
                X86_PAT before anything maps write-combining, entry 1 write-combining
                and entries 0 and 3 write-back and uncacheable as at power-on; and a
                leaf the kernel maps write-combining shall select entry 1 by its
                write-through bit alone, with device memory's uncacheable entry
                winning where both are asked.";
            attribute :>> criterion = "Host tests: a write-combining leaf has PWT
                and not PCD, reads back as itself, and loses to device; X86_PAT's
                entries 0, 1 and 3 are 0x06, 0x01 and 0x00. On x86-64 the boot's
                cpu line says entry 1 is write-combining, and every processor read
                its PAT back, or stage 9's check fails.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::x86_64::cpu::program_pat",
                "arch::x86_64::write_combining_processors", "arch::x86_64::init_traps",
                "arch::x86_64::smp::secondary_start");
        }
        requirement <'L.x86_64.32'> speculationPlanFollowsCpuid : ItemLowLevel {
            attribute :>> statement = "Plan::for_processor shall choose enhanced
                IBRS where IA32_ARCH_CAPABILITIES offers it, else AutoIBRS, else
                IBRS; STIBP unless enhanced IBRS; SSBD unless the processor reports
                it is not affected; the IBPB barrier where IBPB is offered; VERW on
                exit only for an Intel processor with MD_CLEAR and without MDS_NO;
                and offered shall decode those bits from CPUID and
                IA32_ARCH_CAPABILITIES.";
            attribute :>> criterion = "For each row of a table of CPUID and
                capability words, the plan's defences and MSR values are the
                expected set.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::offered",
                "arch::x86_64::speculation::Plan::for_processor",
                "arch::x86_64::speculation::store_bypass_immune",
                "arch::x86_64::speculation::mds_exposed");
        }
        requirement <'L.x86_64.33'> speculationExposureReported : ItemLowLevel {
            attribute :>> statement = "report_exposure shall print one `cpu` line
                saying, for Spectre v2 into ring 0, between programs, store bypass
                and Meltdown, whether the plan covers it, the processor is not
                affected, or it is uncovered (AoU-11).";
            attribute :>> criterion = "Under TCG's qemu64 the line reports Spectre
                v2 into ring 0 not covered and Meltdown not affected; under KVM with
                auto-IBRS offered it reports it covered.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::report_exposure",
                "arch::x86_64::speculation::meltdown_exposed",
                "arch::x86_64::speculation::store_bypass_immune");
        }
    }

    package Timers {
        doc /* src/kernel/src/arch/x86_64/clock.rs and apic.rs's timer and I/O APICs: the
             * counter, its rate, and the timer interrupt. */

        requirement <'L.x86_64.34'> lapicTimerCalibrated : ItemLowLevel {
            attribute :>> statement = "init shall enable the boot processor's local
                APIC at task priority 0, and calibrate shall measure its timer's
                rate over 10 ms of the counter, so that arm(n) fires n ns later by
                the counter.";
            attribute :>> criterion = "250 ticks of a 1 ms periodic timer, timed by
                the counter, measure within 25% of 1000 Hz, with 0 unclaimed
                interrupts.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::apic::init",
                "arch::x86_64::apic::calibrate",
                "arch::x86_64::apic::arm");
        }
        requirement <'L.x86_64.35'> lapicTimerOneShot : ItemLowLevel {
            attribute :>> statement = "arm shall fire the timer interrupt once, at
                least one tick later, and disarm shall stop it and mask its entry.";
            attribute :>> criterion = "A 1 ms one-shot fires, and does not fire
                again over the following 10 ms.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::apic::arm", "arch::x86_64::apic::disarm");
        }
        requirement <'L.x86_64.36'> ioApicInputsQuiesced : ItemLowLevel {
            attribute :>> statement = "quiesce_io_apics shall mask every redirection
                entry of every I/O APIC the MADT describes.";
            attribute :>> criterion = "After interrupt bring-up every redirection
                entry nothing routed since reads back masked, on every I/O APIC: 0
                unmasked.";
            attribute :>> parent = "H.IRQ.3";
            attribute :>> unit = ("arch::x86_64::apic::quiesce_io_apics",
                "arch::x86_64::apic::mask_all_inputs",
                "arch::x86_64::apic::io_apic_write",
                "arch::x86_64::apic::io_apic_read");
        }
        requirement <'L.x86_64.37'> isaLineRouted : ItemLowLevel {
            attribute :>> statement = "IoApicInput::for_isa shall find the I/O APIC
                input the MADT's override puts an ISA IRQ on, with its polarity and
                trigger, or refuse where no I/O APIC covers it; route shall write
                the destination before the vector, the signalling bits and the mask.";
            attribute :>> criterion = "The serial port's input reads back its vector
                and this processor's APIC ID, masked until enable_console_receive
                and unmasked after, and a byte sent to the port is taken by that
                interrupt.";
            attribute :>> parent = "H.IRQ.3";
            attribute :>> unit = ("arch::x86_64::apic::IoApicInput::for_isa",
                "arch::x86_64::apic::IoApicInput::route");
        }
        requirement <'L.x86_64.38'> counterFrequencyKnown : ItemLowLevel {
            attribute :>> statement = "clock::init shall bring up a 64-bit counter
                and its frequency: the invariant TSC measured over 10 ms of the HPET
                where both exist, else a 64-bit HPET at its stated period, else the
                TSC measured against a 32-bit HPET, else against PIT channel 2.";
            attribute :>> criterion = "counter_hz agrees with a reference the
                counter was not calibrated from, within a stated tolerance, on each
                path a gate boots.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::init",
                "arch::x86_64::clock::hpet_address",
                "arch::x86_64::clock::start_hpet",
                "arch::x86_64::clock::invariant_tsc",
                "arch::x86_64::clock::calibrate_tsc_against_hpet",
                "arch::x86_64::clock::start_tsc",
                "arch::x86_64::clock::pit_finished",
                "arch::x86_64::clock::calibrate_tsc",
                "arch::x86_64::clock::write64",
                "arch::x86_64::clock::main_counter",
                "arch::x86_64::clock::read64",
                "arch::x86_64::cpu::rdtsc",
                "arch::x86_64::cpu::outb",
                "arch::x86_64::cpu::inb");
        }
        requirement <'L.x86_64.39'> counterBringUpRefusesDeadTimer : ItemLowLevel {
            attribute :>> statement = "clock::init shall fail with a named reason,
                not answer a counter, when the HPET's counter does not advance or
                the PIT never reaches its terminal count.";
            attribute :>> criterion = "With the HPET's counter held still, and with
                no HPET and the PIT's output held low, init answers an error, 2 of
                2.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::start_hpet",
                "arch::x86_64::clock::calibrate_tsc",
                "arch::x86_64::clock::start_tsc",
                "arch::x86_64::clock::pit_finished");
        }
        requirement <'L.x86_64.40'> counterNeverBackwards : ItemLowLevel {
            attribute :>> statement = "counter_now shall answer the TSC, or the
                HPET's main counter read whole across a 32-bit carry, never
                decreasing on one processor.";
            attribute :>> criterion = "N consecutive counter_now reads on each
                online processor never decrease.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::counter_now",
                "arch::x86_64::clock::main_counter");
        }
        requirement <'L.x86_64.41'> vdsoReadsCounterOnlyWhenTsc : ItemLowLevel {
            attribute :>> statement = "counter_is_tsc shall answer true only when
                the counter counter_now reads is the TSC, at counter_hz, so that the
                vDSO reads the clock itself only then.";
            attribute :>> criterion = "The vDSO program exits 120: each of the 7
                clocks, gettimeofday and time through the vDSO answers between
                system calls made on either side.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = "arch::x86_64::clock::counter_is_tsc";
        }
    }

    package SignalFrames {
        doc /* src/kernel/src/arch/x86_64/signal.rs: a 64-bit program's signal frame, its
             * return, and the context resumed from it. */

        requirement <'L.x86_64.42'> nativeSignalRoundTrip : ItemLowLevel {
            attribute :>> statement = "setup_signal_frame shall, for a 64-bit
                program whose handler names a restorer, write Linux's rt_sigframe
                below the stack -- pretcode the restorer, a ucontext holding the
                interrupted registers and mask, the siginfo -- and enter the handler
                with RDI the signal, RSI the siginfo and RDX the ucontext;
                restore_signal_frame shall load the registers and mask that frame
                holds, a register the handler changed through the ucontext included.";
            attribute :>> criterion = "The 64-bit signal program exits 77: its
                SA_SIGINFO handler for SIGUSR1 saw EDI 10, si_signo 10 at RSI and
                SIGUSR1 blocked, the 70 it added to the ucontext's saved RBX came
                back through rt_sigreturn, and the mask after the return is empty
                (the `signals` line).";
            attribute :>> parent = "H.TRAP.9";
            attribute :>> unit = ("arch::x86_64::signal::setup_signal_frame",
                "arch::x86_64::signal::frame_is_buildable",
                "arch::x86_64::signal::place",
                "arch::x86_64::signal::write_fp_area",
                "arch::x86_64::signal::UserContext::registers",
                "arch::x86_64::signal::UserContext::store_syscall",
                "arch::x86_64::signal::restore_signal_frame",
                "arch::x86_64::signal::restore_fpu",
                "arch::x86_64::signal::UserContext::set_registers",
                "arch::x86_64::signal::resume_context");
        }
        requirement <'L.x86_64.43'> nativeForgedFrameRefused : ItemLowLevel {
            attribute :>> statement = "restore_signal_frame shall refuse a 64-bit
                frame whose RIP or RSP lies outside the user half, and resume a
                frame's program with RFLAGS limited to the restorable flags and IF,
                and CS and SS ring 3's, whatever the frame holds.";
            attribute :>> criterion = "64-bit frames with a kernel-half RIP and with
                a kernel-half RSP end with SIGSEGV; frames asking IOPL 3, IF clear,
                TF, and CS and SS 0x10 and 0x18 resume in ring 3 with IOPL 0 and IF
                set, read by pushfq and by moving CS and SS.";
            attribute :>> parent = "H.TRAP.5";
            attribute :>> unit = "arch::x86_64::signal::restore_signal_frame";
        }
        requirement <'L.x86_64.44'> sanitisedContext : ItemLowLevel {
            attribute :>> statement = "UserContext::sanitised shall leave CS 64- or
                32-bit user code, SS user data and RFLAGS the restorable flags and
                IF, whatever the trap frame held.";
            attribute :>> criterion = "A context with CS 0x10, SS 0x18 and RFLAGS
                0x3100 comes back with user code, user data and RFLAGS 0x202.";
            attribute :>> parent = "H.TRAP.5";
            attribute :>> unit = "arch::x86_64::signal::UserContext::sanitised";
        }
        requirement <'L.x86_64.45'> fpStateRoundTrip : ItemLowLevel {
            attribute :>> statement = "A handler's frame shall carry the interrupted
                x87 and SSE state, and its return shall reload it, MXCSR masked to
                the processor's MXCSR_MASK.";
            attribute :>> criterion = "A value in XMM0 and in the x87 control word,
                clobbered by the handler, reads back after the return for each ABI;
                a frame with reserved MXCSR bits set returns without a #GP.";
            attribute :>> parent = "H.TRAP.9";
            attribute :>> unit = ("arch::x86_64::signal::write_fp_area",
                "arch::x86_64::signal::restore_fpu",
                "arch::x86_64::signal::compat::write_fpstate");
        }
        requirement <'L.x86_64.46'> frameRefusedAtSetup : ItemLowLevel {
            attribute :>> statement = "setup_signal_frame shall refuse a frame for a
                handler installed without SA_RESTORER, and for a 32-bit program
                whose frame stack lies above 4 GiB.";
            attribute :>> criterion = "A program that signals itself with such a
                handler is ended with SIGSEGV, 1 of 1 per ABI.";
            attribute :>> parent = "H.TRAP.9";
            attribute :>> unit = ("arch::x86_64::signal::frame_is_buildable",
                "arch::x86_64::signal::compat::place");
        }
    }

    package CompatSignalFrames {
        doc /* src/kernel/src/arch/x86_64/signal/compat.rs: an i386 program's two signal
             * frames and their returns. */

        requirement <'L.x86_64.47'> compatSignalRoundTrip : ItemLowLevel {
            attribute :>> statement = "setup_signal_frame shall, for a 32-bit
                program, write rt_sigframe_ia32 for an SA_SIGINFO handler and
                sigframe_ia32 for a plain one and enter the handler on it;
                rt_sigreturn and sigreturn shall load back the saved general
                registers, a saved EAX the handler changed included.";
            attribute :>> criterion = "The i386 signal program exits 42: its
                SA_SIGINFO handler saw signal 10, si_signo 10 and its own pid, and
                its kill answered the 0x77 it wrote to the ucontext's EAX with ESI
                0x5eed intact; its plain handler saw 12 and its kill answered the
                0x88 it wrote to the sigcontext (the signal `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::signal::compat::setup",
                "arch::x86_64::signal::compat::rt_frame_bytes",
                "arch::x86_64::signal::compat::plain_frame_bytes",
                "arch::x86_64::signal::compat::sigcontext_of",
                "arch::x86_64::signal::compat::put_stack",
                "arch::x86_64::signal::compat::enter_handler",
                "arch::x86_64::signal::compat::restore",
                "arch::x86_64::signal::compat::Saved::read",
                "arch::x86_64::signal::compat::Saved::check",
                "arch::x86_64::signal::compat::Saved::load",
                "arch::x86_64::signal::compat::restored_of",
                "arch::x86_64::signal::compat::stack_at");
        }
        requirement <'L.x86_64.48'> compatForgedFrameRefused : ItemLowLevel {
            attribute :>> statement = "compat::restore shall refuse an rt_sigreturn
                frame whose code selector is kernel or 64-bit code, whose stack
                selector is the kernel's data, whose flags hold IOPL or VM, or whose
                GS, FS or DS names a kernel slot, the LDT or the TSS, so that the
                program ends with SIGSEGV; and shall take a frame changed only in
                its saved EAX.";
            attribute :>> criterion = "Of frames forged with CS 0x10, CS 0x33, SS
                0x18, flags 0x3202, flags 0x2_0202, GS 0x10, FS 0x07 and DS 0x43, 8
                of 8 end with SIGSEGV; the same program with only EAX changed exits
                42.";
            attribute :>> parent = "H.TRAP.5";
            attribute :>> unit = ("arch::x86_64::signal::compat::restore",
                "arch::x86_64::signal::compat::Saved::read",
                "arch::x86_64::signal::compat::Saved::check");
        }
        requirement <'L.x86_64.49'> compatForgedFrameRest : ItemLowLevel {
            attribute :>> statement = "compat::restore shall also refuse a frame
                holding VIF or VIP, or an ES naming a kernel slot, and every forgery
                through plain sigreturn as through rt_sigreturn.";
            attribute :>> criterion = "Forged VIF, VIP and ES frames, and the 8
                forgeries above sent through sigreturn, each end with SIGSEGV.";
            attribute :>> parent = "H.TRAP.5";
            attribute :>> unit = ("arch::x86_64::signal::compat::restore",
                "arch::x86_64::signal::compat::Saved::check");
        }
        requirement <'L.x86_64.50'> compatHandlerView : ItemLowLevel {
            attribute :>> statement = "enter_handler shall enter a 32-bit handler
                with EAX the signal, EDX and ECX the siginfo and ucontext for
                SA_SIGINFO and zero otherwise; sigcontext_of shall record a fault's
                trap number and error code.";
            attribute :>> criterion = "A handler reads these registers, and a
                fault's trap number, and exits with a status saying each matched.";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::signal::compat::enter_handler",
                "arch::x86_64::signal::compat::sigcontext_of");
        }
        requirement <'L.x86_64.51'> compatMaskAndAltstack : ItemLowLevel {
            attribute :>> statement = "compat::restore shall restore the mask an
                i386 frame saved, and for rt_sigreturn its alternate-stack record.";
            attribute :>> criterion = "After each return, rt_sigprocmask reports the
                mask from before the signal, and sigaltstack the frame's record.";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::signal::compat::restored_of",
                "arch::x86_64::signal::compat::put_stack",
                "arch::x86_64::signal::compat::stack_at");
        }
    }

    package SystemCallEntry {
        doc /* src/kernel/src/arch/x86_64/syscall.rs: SYSCALL's entry, the registers a call
             * reads and a fork child resumes from, and entering a program. */

        requirement <'L.x86_64.52'> syscallEntersAtLstar : ItemLowLevel {
            attribute :>> statement = "syscall::init shall enable SYSCALL and point
                IA32_LSTAR at the stub, so that a 64-bit program's SYSCALL enters at
                the stub's first instruction, is dispatched, and returns through its
                sysretq.";
            attribute :>> criterion = "With breakpoints on the stub's first
                instruction and on its sysretq, a program that writes and exits hits
                the first at least twice and the second at least once, and exits 42
                (the first `debug` line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::syscall::init",
                "arch::x86_64::syscall::stub_address",
                "arch::x86_64::syscall::ferrix_syscall_entry");
        }
        requirement <'L.x86_64.53'> programGsBaseNotPerCpu : ItemLowLevel {
            attribute :>> statement = "syscall::init shall park zero in
                IA32_KERNEL_GS_BASE, and the GS base saved and restored for a
                program shall be the program's, so that no program runs with a
                per-processor record as its GS base.";
            attribute :>> criterion = "After a program has run on a processor, the
                program's GS base names no processor's per-processor record.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::syscall::program_gs_base",
                "arch::x86_64::syscall::set_program_gs_base");
        }
        requirement <'L.x86_64.54'> compatModeEntriesRefused : ItemLowLevel {
            attribute :>> statement = "syscall::init shall point IA32_CSTAR at a
                stub answering -ENOSYS and leave IA32_SYSENTER_CS zero, so that
                SYSCALL from compatibility mode answers -ENOSYS, or is #UD on Intel,
                and SYSENTER ends the program.";
            attribute :>> criterion = "A 64-bit program that far-returns to 0x23 and
                makes SYSCALL there exits 0, with -ENOSYS, or is ended by SIGILL;
                one that makes SYSENTER there is ended by SIGSEGV or SIGILL and
                never reaches its exit (the first `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = "arch::x86_64::syscall::cstar_stub_address";
        }
        requirement <'L.x86_64.55'> syscallFirstFourArguments : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry shall dispatch a SYSCALL
                with its number from RAX and its first four arguments from RDI, RSI,
                RDX and R10, and return the result in RAX.";
            attribute :>> criterion = "The 64-bit signal program's rt_sigaction and
                rt_sigprocmask, whose set size 8 is in R10 and is refused EINVAL
                otherwise, each answer 0, and the program exits 77, not 99.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::syscall::ferrix_syscall_entry";
        }
        requirement <'L.x86_64.56'> syscallFifthSixthArguments : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry shall pass R8 and R9 as
                a SYSCALL's fifth and sixth arguments.";
            attribute :>> criterion = "A program maps a memfd with its descriptor in
                R8 and a non-zero page offset in R9 and reads that page's marker.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::syscall::ferrix_syscall_entry";
        }
        requirement <'L.x86_64.57'> syscallFlagMask : ItemLowLevel {
            attribute :>> statement = "syscall::init shall set IA32_FMASK so that
                SYSCALL clears TF, IF, DF, NT, RF and AC on entry.";
            attribute :>> criterion = "A program calling exit_group with TF set
                exits with its status; one with DF and AC set makes a call that
                copies user memory and gets the right answer.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = "arch::x86_64::syscall::init";
        }
        requirement <'L.x86_64.58'> starUserBaseRpl3 : ItemLowLevel {
            attribute :>> statement = "syscall::init shall program STAR so that
                SYSRET leaves SS the user data selector at RPL 3.";
            attribute :>> criterion = "A program reads SS after a system call and
                finds user data at RPL 3.";
            attribute :>> parent = ("H.TRAP.6", "H.TRAP.7");
            attribute :>> unit = "arch::x86_64::syscall::init";
        }
        requirement <'L.x86_64.59'> entryStackPerTask : ItemLowLevel {
            attribute :>> statement = "set_entry_stack and restore_user_state shall
                point both ways in from ring 3 -- the SYSCALL entry stack and the
                TSS's RSP0, which set_privilege_stack finds through STR and SGDT --
                at the incoming task's kernel stack, so that two programs taking
                turns on one processor keep their own registers.";
            attribute :>> criterion = "Two spinning programs pinned to one
                processor, each preempted in user mode at least twice, exit 41 and
                43 with their stack pointers unchanged (the `procs` line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::syscall::set_entry_stack",
                "arch::x86_64::gdt::set_privilege_stack",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::cpu::read_task_register",
                "arch::x86_64::cpu::read_gdt");
        }
        requirement <'L.x86_64.60'> archPrctlRefusals : ItemLowLevel {
            attribute :>> statement = "arch_prctl shall refuse ARCH_SET_FS with an
                address outside the user half with EPERM, and any other request with
                EINVAL.";
            attribute :>> criterion = "A program's ARCH_SET_FS with
                0xffff800000000000 answers -1 and its ARCH_GET_FS -22, and it exits
                0 (the `prctl` line).";
            attribute :>> parent = "H.TRAP.1";
            attribute :>> unit = "arch::x86_64::syscall::arch_prctl";
        }
        requirement <'L.x86_64.61'> threadPointerPerTask : ItemLowLevel {
            attribute :>> statement = "arch_prctl(ARCH_SET_FS) with a user address
                shall set this processor's FS base and the running task's recorded
                one, with interrupts masked, and restore_user_state shall load each
                task's recorded FS base and program GS base where its selector is
                null; a base a null selector cleared comes back as recorded.";
            attribute :>> criterion = "Two 64-bit programs pinned to one processor
                each set a distinct FS base and read their own words through %fs
                10,000 times across sched_yield, and one that clears its base with
                USER_DS and a null selector reads its own word again after a
                nanosleep (the `fsbase` line); with the save reading FS_BASE again,
                that read faults and the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::syscall::arch_prctl",
                "arch::x86_64::syscall::set_thread_pointer",
                "arch::x86_64::syscall::thread_pointer",
                "arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::load_selectors");
        }
        requirement <'L.x86_64.62'> forkChildRegistersCopied : ItemLowLevel {
            attribute :>> statement = "UserRegs::for_child shall copy a SYSCALL
                frame with every register the parent's but RAX, which is zero, and
                UserRegs::stack_pointer shall answer its user RSP.";
            attribute :>> criterion = "A frame of 16 distinct values copied for a
                child prints all 16 by name, RAX 0 and the other 15 unchanged, and
                its stack pointer is the parent's (the `regs` line).";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = ("arch::x86_64::syscall::UserRegs::for_child",
                "arch::x86_64::syscall::UserRegs::stack_pointer");
        }
        requirement <'L.x86_64.63'> nativeForkChildResumes : ItemLowLevel {
            attribute :>> statement = "resume_user shall resume a 64-bit fork child
                from its copied SYSCALL frame through SYSRET, after the call, with
                the call answering zero.";
            attribute :>> criterion = "The 64-bit fork program's child sees fork
                answer 0 and exits 23; the parent's wait4 reaps it and the parent
                exits 24 (the `fork` line).";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::syscall::resume_user";
        }
        requirement <'L.x86_64.64'> compatForkChildResumes : ItemLowLevel {
            attribute :>> statement = "system_call shall hand the dispatcher the
                trap frame as the caller's registers, and resume_user shall resume
                the child of a fork made through int $0x80 from it through an IRETQ,
                after the call, with EAX zero.";
            attribute :>> criterion = "The i386 signal program's fork child exits 23
                and wait4 answers its pid with status 23, and the program exits 42.";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::syscall::resume_user",
                "arch::x86_64::system_call");
        }
        requirement <'L.x86_64.65'> cloneStack : ItemLowLevel {
            attribute :>> statement = "UserRegs::set_stack shall start a clone
                child, from a SYSCALL or an int $0x80 frame, on the stack clone
                names.";
            attribute :>> criterion = "A thread made with a stack argument reads its
                stack pointer on entry and finds it at that stack's top, for each
                ABI.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::syscall::UserRegs::set_stack";
        }
        requirement <'L.x86_64.66'> nativeRestartRewinds : ItemLowLevel {
            attribute :>> statement = "rewind_syscall shall, for a SYSCALL
                restarting without a restart block, point RIP back at the syscall
                instruction and put the call's number back in RAX, so that the call
                is made again.";
            attribute :>> criterion = "A FUTEX_WAIT with no timeout, stopped by
                SIGSTOP and continued by SIGCONT, never records a return and is back
                on its word (the `threads` stop line).";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = ("arch::x86_64::signal::UserContext::rewind_syscall",
                "arch::x86_64::signal::UserContext::from_syscall");
        }
        requirement <'L.x86_64.67'> restartOtherBranches : ItemLowLevel {
            attribute :>> statement = "rewind_syscall shall restart a call with a
                restart block as restart_syscall, x86-64's number through SYSCALL
                and i386's through int $0x80, and rewind an int $0x80 call by two
                bytes.";
            attribute :>> criterion = "A clock_nanosleep interrupted by a stop
                resumes as restart_syscall and sleeps only the rest, for each entry;
                an i386 futex wait stopped and continued restarts.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::signal::UserContext::rewind_syscall";
        }
        requirement <'L.x86_64.68'> nativeFirstEntryArgument : ItemLowLevel {
            attribute :>> statement = "enter_user shall, for a native program, enter
                ring 3 at its entry point on the stack given, with the start
                argument in RDI.";
            attribute :>> criterion = "A program started with argument 57 exits 57
                (the `argument` line).";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = ("arch::x86_64::enter_user",
                "arch::x86_64::syscall::enter_user");
        }
        requirement <'L.x86_64.69'> nativeExecveEntry : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry shall, when a SYSCALL's
                outcome is to enter a new native image, leave through SYSRET at its
                entry point on its stack.";
            attribute :>> criterion = "A program's execve of the user test program
                ends with that program's status, 42 (the `execve` line).";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = ("arch::x86_64::syscall::ferrix_syscall_entry",
                "arch::x86_64::syscall::enter_program");
        }
        requirement <'L.x86_64.70'> freshEntryClearsRegisters : ItemLowLevel {
            attribute :>> statement = "A program entered fresh, by enter_user or by
                an execve through SYSCALL, shall find every general register zero
                but its start argument and stack pointer, with RFLAGS 0x202.";
            attribute :>> criterion = "A program that ORs its registers on entry
                exits 0, on first start and after execve, for each ABI.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = ("arch::x86_64::syscall::enter_user",
                "arch::x86_64::signal::UserContext::entering");
        }
        requirement <'L.x86_64.71'> compatExecveFromSyscall : ItemLowLevel {
            attribute :>> statement = "enter_compat_after_execve shall start an i386
                image that a 64-bit program execve'd through SYSCALL, in
                compatibility mode.";
            attribute :>> criterion = "A 64-bit program execve's the i386 hello
                program, which exits 42.";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = "arch::x86_64::syscall::enter_compat_after_execve";
        }
        requirement <'L.x86_64.124'> theFilterIsAskedBeforeAnyAnswerOfTheEntrys : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry and system_call shall ask
                the registered filter about a call before arch_prctl, sigreturn and
                rt_sigreturn are answered and before the dispatcher is, with the
                number's low 32 bits, the instruction after the call, and the token
                of the entry: x86-64's for SYSCALL and i386's for int $0x80, whatever
                the image; and shall apply the filter's errno as the dispatcher's answer.";
            attribute :>> criterion = "A test-only filter is shown arch_prctl and
                rt_sigreturn through SYSCALL and sigreturn and rt_sigreturn through
                int $0x80 once each, before their answers, and its value comes back; a
                filter about x86-64 alone does not judge an int $0x80 call.";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = ("arch::x86_64::audit_arch",
                "arch::x86_64::syscall::answer_here");
        }
        requirement <'L.x86_64.125'> aRolledBackFrameReadsAsAtTheCall : ItemLowLevel {
            attribute :>> statement = "syscall_rollback_value shall give the value
                for RAX that makes the frame of a call that did not run read as it did
                at the call: the number.";
            attribute :>> criterion = "The value for number 0x2222 is 0x2222 (the
                `seccomp` line's rollback check); the handler of a trapped call reads
                it from a real frame in a later landing.";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = "arch::x86_64::syscall_rollback_value";
        }
        requirement <'L.x86_64.140'> theEntrySortsANumberOnce : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry shall test a number
                against the native range before any Linux table, pass a native
                number on to the dispatcher without decoding it against x86-64's
                table, and answer arch_prctl and rt_sigreturn from one decode of a
                Linux number behind its clamp.";
            attribute :>> criterion = "Every number of the native range, 0x1000 +
                158 (arch_prctl's low bits) among them, sorts as native, the numbers
                either side as Linux numbers the table does not hold, and 158 and 15
                as arch_prctl and rt_sigreturn (the `decode` line); with the native
                test removed the line fails.";
            attribute :>> parent = ("H.TRAP.3", "H.TRAP.12");
            attribute :>> unit = ("arch::x86_64::syscall::early",
                "arch::x86_64::syscall::ferrix_syscall_entry");
        }
    }

    package CompatEntry {
        doc /* src/kernel/src/arch/x86_64/mod.rs and trap.rs: int $0x80, and which mode an
             * image runs in. */

        requirement <'L.x86_64.72'> int80IsAnI386Call : ItemLowLevel {
            attribute :>> statement = "An int $0x80 from ring 3 shall be answered as
                an i386 system call: through a gate ring 3 may use, classified as a
                system call, its number from EAX and arguments from EBX, ECX and EDX
                as their low 32 bits, decoded through the i386 table, its result in
                EAX.";
            attribute :>> criterion = "An ELF32 i386 program's write(1, msg, 16)
                answers 16, write(-1) answers -9 in EAX, number 90 answers -38, and
                it exits 42; a 64-bit program's int $0x80 getpid equals its SYSCALL
                getpid in all of RAX, and dup(0x1_0000_0001) duplicates descriptor 1
                (the first `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::trap::init",
                "arch::x86_64::trap::classify",
                "arch::x86_64::system_call",
                "arch::x86_64::decode_compat_syscall",
                "arch::x86_64::syscall::enter_user",
                "arch::x86_64::signal::UserContext::entering",
                "arch::x86_64::signal::resume_context",
                "arch::x86_64::image_abi");
        }
        requirement <'L.x86_64.73'> i386SignalsDeliveredOnTheWayBack : ItemLowLevel {
            attribute :>> statement = "system_call shall answer sigreturn and
                rt_sigreturn by restoring the frame the handler left rather than
                dispatching them, and dispatch shall deliver a pending signal on the
                way back to user mode.";
            attribute :>> criterion = "The i386 signal program's two handlers are
                entered and their kills answer the EAX each wrote into its frame,
                and the program exits 42 (the signal `i386` line).";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = ("arch::x86_64::system_call",
                "trap::dispatch",
                "trap::return_path");
        }
        requirement <'L.x86_64.74'> aForeignImageIsRefused : ItemLowLevel {
            attribute :>> statement = "image_abi shall answer no ABI for an image of
                another machine, so that the loader refuses it.";
            attribute :>> criterion = "An ELF64 image built for another machine is
                refused by the loader.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::image_abi";
        }
        requirement <'L.x86_64.75'> anI386ExecveEntersCompatMode : ItemLowLevel {
            attribute :>> statement = "system_call shall start the image an i386
                program execve's through int $0x80 in its mode.";
            attribute :>> criterion = "An i386 program execve's an i386 image
                through int $0x80, which runs and exits with its status.";
            attribute :>> parent = "H.TRAP.8";
            attribute :>> unit = "arch::x86_64::system_call";
        }
        requirement <'L.x86_64.76'> auxvPlatform : ItemLowLevel {
            attribute :>> statement = "user_platform shall name the program's
                platform as Linux does: x86_64 for a 64-bit program and i686 for an
                i386 one.";
            attribute :>> criterion = "AT_PLATFORM reads x86_64 for a 64-bit program
                and i686 for an i386 one.";
            attribute :>> parent = "H.TRAP.10";
            attribute :>> unit = "arch::x86_64::user_platform";
        }
    }

    package Exceptions {
        doc /* src/kernel/src/arch/x86_64/trap.rs and src/kernel/src/trap.rs: the gates, and
             * what a fault becomes. */

        requirement <'L.x86_64.77'> userExceptionsNameLinuxSignals : ItemLowLevel {
            attribute :>> statement = "init_traps and trap::init shall install and
                load a gate for every exception vector, and classify and
                fault_signal shall name for an exception a user-mode instruction
                took the signal Linux sends: #DE SIGFPE, #UD SIGILL, #PF on an
                unmapped address SIGSEGV, #GP SIGSEGV, #MF SIGFPE.";
            attribute :>> criterion = "Programs that divide by zero, run ud2, read
                an unmapped address, run hlt in ring 3 and take an unmasked x87
                zero-divide end with SIGFPE, SIGILL, SIGSEGV, SIGSEGV and SIGFPE, 5
                of 5, and the boot goes on (the `fault` line).";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::x86_64::init_traps",
                "arch::x86_64::trap::init",
                "arch::x86_64::trap::load_on_this_cpu",
                "arch::x86_64::trap::ferrix_trap_entry",
                "arch::x86_64::trap::classify",
                "arch::x86_64::trap::fault_signal",
                "arch::x86_64::cpu::read_cr0");
        }
        requirement <'L.trap.1'> aUserFaultBecomesItsSignal : ItemLowLevel {
            attribute :>> statement = "dispatch shall answer a breakpoint, illegal
                instruction or other fault taken from user mode by forcing the
                architecture's signal on the program through the registered return
                path, and shall not stop the kernel.";
            attribute :>> criterion = "The faulting programs of the `fault` line are
                ended by their signals and the check returns to the boot.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("trap::dispatch",
                "trap::user_fault_as",
                "trap::return_path");
        }
        requirement <'L.x86_64.78'> aTouchPastAFileIsSigbus : ItemLowLevel {
            attribute :>> statement = "handle_page_fault shall answer with SIGBUS a
                user fault the address space reports as past the end of a mapped
                file.";
            attribute :>> criterion = "A program that reads the second page of a
                two-page shared mapping of a one-page memfd ends with SIGBUS (the
                `fault` line).";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("trap::handle_page_fault",
                "trap::resolve_user_fault",
                "trap::user_fault_as",
                "arch::x86_64::cpu::read_cr2");
        }
        requirement <'L.trap.2'> aUserFaultInAMappedRegionIsResolved : ItemLowLevel {
            attribute :>> statement = "handle_page_fault shall resolve a user-mode
                fault in a region the running program's space maps by making the
                mapping true, so that the instruction is retried.";
            attribute :>> criterion = "The user test program runs with at least 1
                page fault resolved and exits 42.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("trap::handle_page_fault", "trap::resolve_user_fault");
        }
        requirement <'L.x86_64.79'> userInterruptsPreempt : ItemLowLevel {
            attribute :>> statement = "dispatch shall count an interrupt taken from
                user mode, have the architecture service it and signal its end, and
                then preempt the program.";
            attribute :>> criterion = "Two spinning programs pinned to one processor
                are each preempted at least twice and switched to at least twice,
                and the count of interrupts taken in user mode rises while they run.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("trap::dispatch", "arch::x86_64::service_interrupts");
        }
        requirement <'L.x86_64.80'> paranoidVectorsRouted : ItemLowLevel {
            attribute :>> statement = "trap::init shall route the debug exception
                and the NMI to the paranoid entry.";
            attribute :>> criterion = "An NMI sent to this processor with interrupts
                masked is taken at least once with a GS naming a processor's record,
                and hardware breakpoints on the SYSCALL entry and return fire with
                the kernel's GS while the program exits 42 (the `nmi` and `debug`
                lines).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::trap::init",
                "arch::x86_64::trap::load_on_this_cpu");
        }
        requirement <'L.x86_64.81'> onlyInt80IsUserCallable : ItemLowLevel {
            attribute :>> statement = "trap::init shall let ring 3 raise no vector
                but int $0x80 and the breakpoint.";
            attribute :>> criterion = "A program running int $0x81 and int $14 is
                ended with SIGSEGV, 2 of 2.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = "arch::x86_64::trap::init";
        }
        requirement <'L.x86_64.82'> userBreakpointsAndRareFaults : ItemLowLevel {
            attribute :>> statement = "fault_signal and ferrix_trap_entry shall end
                a program's int3 and a trap-flag single step with SIGTRAP, clearing
                DR6, and an alignment check with SIGBUS.";
            attribute :>> criterion = "A program's int3 and its single step each end
                it with SIGTRAP, and an #AC with SIGBUS.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::x86_64::trap::fault_signal",
                "arch::x86_64::trap::ferrix_trap_entry");
        }
        requirement <'L.x86_64.83'> doubleFaultHasItsOwnStack : ItemLowLevel {
            attribute :>> statement = "trap::init shall give the double fault and
                the machine check their own interrupt stacks, so that a kernel stack
                overflow is taken as a double fault on its own stack.";
            attribute :>> criterion = "A deliberate kernel stack overflow ends in
                the FX-9004 report.";
            attribute :>> parent = "H.FAIL.2";
            attribute :>> unit = "arch::x86_64::trap::init";
        }
        requirement <'L.x86_64.84'> kernelBreakpointsReturn : ItemLowLevel {
            attribute :>> statement = "dispatch shall step a kernel breakpoint's
                saved instruction pointer past it, count it, and return.";
            attribute :>> criterion = "Two kernel int3s each return, the count rises
                by 2, and a register the handler must preserve comes back intact.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("trap::dispatch", "arch::x86_64::trap::classify");
        }
        requirement <'L.x86_64.85'> aFatalTrapIsReported : ItemLowLevel {
            attribute :>> statement = "report, fatal, report_trap and vector_name
                shall print a trap the kernel cannot continue past -- its headline,
                vector, faulting address and saved registers -- with its catalogue
                entry, and stop.";
            attribute :>> criterion = "A boot that faults in the kernel on purpose
                ends with `FERRIX-PANIC page fault at` its address, the vector, CR2
                and register lines, and the catalogue entry.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("trap::report",
                "arch::x86_64::trap::report_trap",
                "arch::x86_64::trap::vector_name");
        }
        requirement <'L.trap.3'> demandWindowFaultsAreMapped : ItemLowLevel {
            attribute :>> statement = "handle_page_fault shall resolve a kernel
                fault inside the on-demand window by mapping a zeroed page and
                retrying.";
            attribute :>> criterion = "Three touches of the demand window are
                resolved, each page read back zero, within the window's frames.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "trap::handle_page_fault";
        }
    }

    package Processor {
        doc /* src/kernel/src/arch/x86_64/mod.rs and cpu.rs: per-processor state, the user
             * root, interrupts, and how a run ends. */

        requirement <'L.x86_64.86'> eachProcessorReachesItsOwnRecord : ItemLowLevel {
            attribute :>> statement = "set_cpu_local shall install the processor's
                own per-processor record as its GS base, and cpu_local and
                cpu_local_register shall read that address back.";
            attribute :>> criterion = "Over 100 rounds on every online processor,
                the record reached through the register names the processor's own
                APIC ID: 0 misplaced (the `smp` line).";
            attribute :>> parent = "H.SCHED.6";
            attribute :>> unit = ("arch::x86_64::set_cpu_local",
                "arch::x86_64::cpu_local",
                "arch::x86_64::cpu_local_register",
                "arch::x86_64::cpu::write_msr",
                "arch::x86_64::cpu::read_gs_word");
        }
        requirement <'L.x86_64.87'> syscallIsArmedWhereAProgramRuns : ItemLowLevel {
            attribute :>> statement = "set_cpu_local shall arm SYSCALL on the
                processor it runs on.";
            attribute :>> criterion = "A program pinned to the checking processor
                makes write and exit_group by SYSCALL and exits 42.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = "arch::x86_64::set_cpu_local";
        }
        requirement <'L.x86_64.88'> aUserRootTranslatesItsHalf : ItemLowLevel {
            attribute :>> statement = "prepare_user_root shall make a new root share
                the kernel's upper half, and install_user_root shall make this
                processor translate its user half through that root.";
            attribute :>> criterion = "Installed twice across an uninstall, a
                space's 2 faulted pages each read back what the processor wrote
                through the user address, and the write is found through the direct
                map at the frame the walk names.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::x86_64::prepare_user_root",
                "arch::x86_64::install_user_root");
        }
        requirement <'L.x86_64.89'> uninstallReturnsToTheKernelRoot : ItemLowLevel {
            attribute :>> statement = "uninstall_user_root shall return this
                processor to the kernel's own root.";
            attribute :>> criterion = "After uninstall, CR3 is the kernel's root and
                the user address no longer translates.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "arch::x86_64::uninstall_user_root";
        }
        requirement <'L.x86_64.90'> ringZeroIsKeptOutOfUserPages : ItemLowLevel {
            attribute :>> statement = "init_traps shall set SMEP, SMAP and UMIP in
                CR4 where the processor offers them.";
            attribute :>> criterion = "CR4 reads back SMEP, SMAP and UMIP set on
                each online processor that offers them.";
            attribute :>> parent = "H.MEM.3";
            attribute :>> unit = ("arch::x86_64::init_traps",
                "arch::x86_64::cpu::enable_user_access_protection",
                "arch::x86_64::cpu::read_cr4",
                "arch::x86_64::cpu::write_cr4");
        }
        requirement <'L.x86_64.91'> theTimerWakesASleeper : ItemLowLevel {
            attribute :>> statement = "init_interrupts shall bring up the counter
                and the local APIC timer so that a timed sleep is woken.";
            attribute :>> criterion = "A 20 ms sleep returns after at least 20 ms
                and at most 400 ms.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = "arch::x86_64::init_interrupts";
        }
        requirement <'L.x86_64.92'> irqMaskingNests : ItemLowLevel {
            attribute :>> statement = "Irq::disable shall mask interrupts and answer
                whether they were open, and Irq::restore shall open them only when
                that answer says they were.";
            attribute :>> criterion = "With interrupts open, disable answers open, a
                nested disable answers closed, the inner restore leaves them closed
                and the outer one opens them.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("arch::x86_64::Irq::disable",
                "arch::x86_64::Irq::restore",
                "arch::x86_64::cpu::read_rflags",
                "arch::x86_64::cpu::disable_interrupts",
                "arch::x86_64::cpu::enable_interrupts",
                "arch::x86_64::cpu::flush_tlb_including_global",
                "arch::x86_64::cpu::load_user_gs");
        }
        requirement <'L.x86_64.93'> anIpiReachesOneProcessor : ItemLowLevel {
            attribute :>> statement = "send_ipi_to shall interrupt the processor
                with the APIC ID it names, and refuse an ID above 255.";
            attribute :>> criterion = "For each online secondary, send_ipi_to its ID
                raises its IPI count and no other's, and ID 256 is refused.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = "arch::x86_64::send_ipi_to";
        }
        requirement <'L.x86_64.94'> noControllerLineToMask : ItemLowLevel {
            attribute :>> statement = "mask_interrupt and unmask_interrupt shall
                refuse every line on x86-64, whose devices mask their own MSI-X
                entries.";
            attribute :>> criterion = "Both answer an error for any line number.";
            attribute :>> parent = "H.IRQ.2";
            attribute :>> unit = ("arch::x86_64::mask_interrupt",
                "arch::x86_64::unmask_interrupt");
        }
        requirement <'L.x86_64.95'> spuriousVectorIsNotAcknowledged : ItemLowLevel {
            attribute :>> statement = "service_interrupts shall signal no end of
                interrupt for the spurious vector.";
            attribute :>> criterion = "A spurious interrupt provoked on purpose is
                answered without an EOI, and the next real interrupt is delivered.";
            attribute :>> parent = "H.IRQ.3";
            attribute :>> unit = "arch::x86_64::service_interrupts";
        }
        requirement <'L.x86_64.96'> consoleInputByInterrupt : ItemLowLevel {
            attribute :>> statement = "console_receive_irq and
                enable_console_receive shall route the serial port's receive
                interrupt, so that a byte typed arrives through the ring by
                interrupt.";
            attribute :>> criterion = "On q35 the receive interrupt is installed,
                and a byte typed on the serial port arrives by interrupt, not by
                polling.";
            attribute :>> parent = "H.IRQ.3";
            attribute :>> unit = ("arch::x86_64::console_receive_irq",
                "arch::x86_64::enable_console_receive",
                "arch::x86_64::msi::allocate_vector");
        }
        requirement <'L.x86_64.97'> theConsoleCarriesTheBoot : ItemLowLevel {
            attribute :>> statement = "init_console shall bring up the 16550 so that
                the kernel's lines reach the serial port.";
            attribute :>> criterion = "cargo xtask test-boot reads FERRIX-BOOT-OK
                from the x86-64 guest's serial output.";
            attribute :>> parent = "H.BOOT.2";
            attribute :>> unit = ("arch::x86_64::init_console",
                "arch::x86_64::console::init",
                "arch::x86_64::console::write_register",
                "arch::x86_64::console::read_register",
                "arch::x86_64::console::write_byte",
                "arch::x86_64::console::transmit_room",
                "arch::x86_64::cpu::outb",
                "arch::x86_64::cpu::inb");
        }
        requirement <'L.x86_64.98'> theMachinePowersItselfOff : ItemLowLevel {
            attribute :>> statement = "shutdown shall drain the console and power
                the machine off.";
            attribute :>> criterion = "After init exits, QEMU exits by itself within
                the grace, with status 0 or 33, and init's exit line is in the log.";
            attribute :>> parent = "H.BOOT.6";
            attribute :>> unit = ("arch::x86_64::shutdown",
                "arch::x86_64::cpu::debug_exit",
                "arch::x86_64::cpu::outl");
        }
        requirement <'L.x86_64.99'> theMachineResets : ItemLowLevel {
            attribute :>> statement = "reset shall restart the machine from firmware
                after the console has drained.";
            attribute :>> criterion = "Under ferrix.onexit=reset the kernel says it
                is resetting and the loader's banner is printed again after it.";
            attribute :>> parent = "H.BOOT.7";
            attribute :>> unit = ("arch::x86_64::reset",
                "arch::x86_64::settle",
                "arch::x86_64::settle_briefly",
                "arch::x86_64::record_reset_register",
                "arch::x86_64::cpu::outb",
                "arch::x86_64::cpu::inb",
                "arch::x86_64::cpu::disable_interrupts");
        }
        requirement <'L.x86_64.100'> aStoppedProcessorStaysStopped : ItemLowLevel {
            attribute :>> statement = "halt shall stop the processor for good.";
            attribute :>> criterion = "After the FX-1501 panic nothing more is
                printed and QEMU does not exit by itself.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::halt",
                "arch::x86_64::cpu::hlt",
                "arch::x86_64::cpu::disable_interrupts");
        }
        requirement <'L.x86_64.101'> theVdsoExportsItsClocks : ItemLowLevel {
            attribute :>> statement = "vdso_spec shall describe the vDSO's code with
                __vdso_clock_gettime, __vdso_gettimeofday and __vdso_time and their
                aliases at the offsets the code puts them.";
            attribute :>> criterion = "The image exports each function and alias
                where its code is, and a program calling each answers between two
                system calls: exit 120.";
            attribute :>> parent = "H.SCHED.9";
            attribute :>> unit = "arch::x86_64::vdso::vdso_spec";
        }
        requirement <'L.x86_64.102'> idleProcessorIsWokenByIpi : ItemLowLevel {
            attribute :>> statement = "A secondary that masks interrupts, finds no
                work and waits with enable_interrupts_and_halt shall be woken by an
                interrupt sent to it.";
            attribute :>> criterion = "Every online processor runs the work handed
                out once in each of 100 rounds, and every secondary takes at least 1
                inter-processor interrupt (the `smp` line).";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("arch::x86_64::cpu::disable_interrupts",
                "arch::x86_64::cpu::enable_interrupts_and_halt",
                "arch::x86_64::cpu::enable_interrupts");
        }
        requirement <'L.x86_64.103'> descriptorTablesAreTheKernels : ItemLowLevel {
            attribute :>> statement = "load_gdt, reload_segments, load_tss and
                load_idt shall install the kernel's descriptor tables, segments and
                task state, so that an exception from ring 3 enters the kernel's
                handler for its vector.";
            attribute :>> criterion = "The six faulting programs of the `fault` line
                end with SIGFPE, SIGILL, SIGSEGV, SIGSEGV, SIGFPE and SIGBUS, 6 of
                6, and the boot goes on.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::x86_64::cpu::load_gdt",
                "arch::x86_64::cpu::reload_segments",
                "arch::x86_64::cpu::load_tss",
                "arch::x86_64::cpu::load_idt");
        }
        requirement <'L.x86_64.104'> bootPrivilegeStackIsSet : ItemLowLevel {
            attribute :>> statement = "gdt's load shall give RSP0 the current stack
                pointer, rounded down to 16 bytes, before any task replaces it.";
            attribute :>> criterion = "Right after gdt::init, the TSS's RSP0 equals
                the stack pointer at that point rounded down to 16.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = "arch::x86_64::cpu::read_stack_pointer";
        }
        requirement <'L.x86_64.105'> backtraceStartsAtCallersFrame : ItemLowLevel {
            attribute :>> statement = "frame_pointer shall answer the calling
                function's frame record, from which a failure report's backtrace
                walks.";
            attribute :>> criterion = "The ferrix.onexit=panic report carries at
                least 1 `trace #0` line that xtask's symbolizer resolves to a kernel
                function.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "arch::x86_64::cpu::frame_pointer";
        }
        requirement <'L.x86_64.106'> tripleFaultResets : ItemLowLevel {
            attribute :>> statement = "triple_fault shall reset the processor by
                loading an empty IDT and raising a breakpoint.";
            attribute :>> criterion = "A --reset boot with no FADT reset register
                and no keyboard controller starts the loader again.";
            attribute :>> parent = "H.BOOT.7";
            attribute :>> unit = "arch::x86_64::cpu::triple_fault";
        }
        requirement <'L.x86_64.107'> hardwareRandomWords : ItemLowLevel {
            attribute :>> statement = "hardware_random shall answer a word from
                RDSEED, or from RDRAND without RDSEED, retrying up to 10 times while
                the carry is clear, and nothing on a processor with neither.";
            attribute :>> criterion = "With RDRAND and RDSEED offered the `random`
                line counts at least 1 word from the processor; with neither, 0.";
            attribute :>> parent = "H.BOOT.3";
            attribute :>> unit = "arch::x86_64::cpu::hardware_random";
        }
    }

    package Memory {
        doc /* src/kernel/src/arch/x86_64/cpu.rs: the control-register and TLB operations
             * the memory code relies on. */

        requirement <'L.x86_64.108'> globalFlushReachesEveryProcessor : ItemLowLevel {
            attribute :>> statement = "flush_tlb_including_global shall drop every
                cached translation on its processor, global ones included, by
                toggling CR4.PGE and reloading CR3.";
            attribute :>> criterion = "20 remaps of a global kernel page to a new
                frame are each seen by every online processor: 0 stale reads (the
                `tlb` line). Under TCG a plain CR3 reload would pass as well; the
                KVM boot row is the one that tells them apart.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("arch::x86_64::cpu::flush_tlb_including_global",
                "arch::x86_64::cpu::read_cr4",
                "arch::x86_64::cpu::write_cr4");
        }
        requirement <'L.x86_64.109'> pageInvalidationDropsOneTranslation : ItemLowLevel {
            attribute :>> statement = "invalidate_page shall drop this processor's
                translation of the page holding an address, global or not.";
            attribute :>> criterion = "A global kernel page read, remapped to
                another frame and invalidated by invalidate_page alone reads the new
                frame's value: 0 stale reads.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = "arch::x86_64::cpu::invalidate_page";
        }
        requirement <'L.x86_64.110'> installedRootIsTranslated : ItemLowLevel {
            attribute :>> statement = "write_cr3 shall make the processor translate
                user addresses through the tables at the root it is given.";
            attribute :>> criterion = "Two tasks pinned to one processor, each in
                its own address space, read one user address 64 times each across
                yields: 128 reads see their own space's value, 0 the other's (the
                `spaces` line).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "arch::x86_64::cpu::write_cr3";
        }
        requirement <'L.x86_64.111'> kernelHonoursReadOnlyMappings : ItemLowLevel {
            attribute :>> statement = "kernel_write_protected shall report CR0.WP,
                so that the boot stops when the kernel could write through a
                read-only mapping.";
            attribute :>> criterion = "Stage 1 reads CR0.WP set, and a kernel write
                through a read-only mapping faults.";
            attribute :>> parent = "H.MEM.5";
            attribute :>> unit = "arch::x86_64::cpu::read_cr0";
        }
        requirement <'L.x86_64.112'> umipKeepsTableAddressesFromRing3 : ItemLowLevel {
            attribute :>> statement = "enable_umip shall set CR4.UMIP where the
                processor offers it, so that ring 3 cannot read the descriptor
                tables' addresses.";
            attribute :>> criterion = "A program executing SIDT and one executing
                SGDT each end with SIGSEGV, 2 of 2.";
            attribute :>> parent = "H.BOOT.3";
            attribute :>> unit = "arch::x86_64::cpu::enable_umip";
        }
    }

    package Interrupts {
        doc /* src/kernel/src/arch/x86_64/msi.rs: message-signalled interrupts. */

        requirement <'L.x86_64.113'> msiMessageRaisesItsVector : ItemLowLevel {
            attribute :>> statement = "msi_allocate shall answer a message address
                and data that, written by a device, raise the vector it allocated on
                the calling processor's local APIC and reach the handler registered
                for it.";
            attribute :>> criterion = "On x86-64 the `pci` line reports at least 1
                entropy request completed by MSI-X.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("arch::x86_64::msi::msi_allocate",
                "arch::x86_64::msi::allocate_vector");
        }
        requirement <'L.x86_64.114'> msiVectorsAreExclusive : ItemLowLevel {
            attribute :>> statement = "allocate_vector shall hand out each of
                vectors 0x40 to 0x7F at most once and answer nothing once all 64 are
                taken, and msi_allocate shall refuse then, and refuse a local APIC
                ID above 255.";
            attribute :>> criterion = "64 allocations answer 64 distinct vectors in
                0x40..0x80; the 65th answers nothing and msi_allocate its error.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("arch::x86_64::msi::allocate_vector",
                "arch::x86_64::msi::msi_allocate");
        }
    }

    package Console {
        doc /* src/kernel/src/arch/x86_64/console.rs: the serial port. */

        requirement <'L.x86_64.115'> consoleReceivesTypedBytes : ItemLowLevel {
            attribute :>> statement = "read_byte shall answer each byte received at
                the serial port once, in the order received, and nothing when none
                waits.";
            attribute :>> criterion = "cargo xtask test-jobs on x86-64 types 18
                steps at the serial port, and each step's answer appears after its
                keystrokes.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("arch::x86_64::console::read_byte",
                "arch::x86_64::console::read_register",
                "arch::x86_64::cpu::inb");
        }
        requirement <'L.x86_64.116'> consoleTransmitsByInterrupt : ItemLowLevel {
            attribute :>> statement = "transmit_interrupt shall enable the serial
                port's transmit-empty interrupt while bytes are queued, so that a
                task's queued write goes out by the interrupt.";
            attribute :>> criterion = "A task's write longer than a 64-byte burst
                leaves at most 64 bytes to the writer, and the rest is sent by the
                transmit interrupt within 2 s.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("arch::x86_64::console::transmit_interrupt",
                "arch::x86_64::console::transmit_room",
                "arch::x86_64::console::write_register");
        }
        requirement <'L.x86_64.117'> consoleKnowsItsFifo : ItemLowLevel {
            attribute :>> statement = "console::init shall record whether the port's
                FIFOs came on, transmit_room shall offer 16 bytes with them and 1
                without, and transmit_buffer shall name which.";
            attribute :>> criterion = "On QEMU's 16550A, transmit_buffer names a
                16550's FIFO and transmit_room answers 0 or 16, never 1.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("arch::x86_64::console::init",
                "arch::x86_64::console::transmit_buffer");
        }
        requirement <'L.x86_64.118'> consoleWaitsAreBounded : ItemLowLevel {
            attribute :>> statement = "write_byte shall wait at most 100000 polls
                for the holding register, and drain shall return once the
                transmitter is idle or after 10000000 polls.";
            attribute :>> criterion = "Against a port that never reports ready,
                write_byte and drain each return; after drain returns on a live
                port, the transmitter reads idle.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::console::write_byte",
                "arch::x86_64::console::drain");
        }
    }

    package Dispatch {
        doc /* src/kernel/src/syscall/mod.rs's dispatcher and src/kernel/src/trap.rs's
             * registration: which calls reach which answer. */

        requirement <'L.trap.4'> anUnknownNumberIsEnosys : ItemLowLevel {
            attribute :>> statement = "system_call shall pass a call to the entry
                set_syscall_entry registered, and dispatch_with shall answer a
                native-entry number that no table carries with -ENOSYS, not a
                handler or a fault.";
            attribute :>> criterion = "0xDEAD, usize::MAX and usize::MAX - 1, each
                dispatched as a native call through the registered entry, are
                answered -38, 3 of 3.";
            attribute :>> parent = "H.TRAP.11";
            attribute :>> unit = ("syscall::dispatch_with",
                "trap::system_call",
                "trap::set_syscall_entry");
        }
        requirement <'L.x86_64.119'> theX8664TableIsCompiledIn : ItemLowLevel {
            attribute :>> statement = "decode_syscall shall decode a number through
                x86-64's own table.";
            attribute :>> criterion = "Of getpid's numbers in the three tables,
                exactly x86-64's decodes to getpid, and dispatching it answers a pid
                above zero.";
            attribute :>> parent = "H.TRAP.12";
            attribute :>> unit = "arch::x86_64::decode_syscall";
        }
        requirement <'L.x86_64.120'> registerAccessesAreVolatile : ItemLowLevel {
            attribute :>> statement = "mmio's read8 to write32 shall each make one
                volatile access of its width at the address given, never elided,
                merged or reordered with another volatile access.";
            attribute :>> criterion = "Every device the boot drives answers through
                them, so each architecture's boot test runs them; no check tells a
                merged access from two.";
            attribute :>> parent = "H.BOOT.2";
            attribute :>> unit = ("arch::x86_64::mmio::read8", "arch::x86_64::mmio::write8",
                "arch::x86_64::mmio::read16", "arch::x86_64::mmio::write16",
                "arch::x86_64::mmio::read32", "arch::x86_64::mmio::write32");
        }
        requirement <'L.x86_64.121'> extendedStateEnabledOnEveryProcessor : ItemLowLevel {
            attribute :>> statement = "enable_extended_state shall, only when CPUID
                reports XSAVE and leaf 0xD supports x87 and SSE, set CR4.OSXSAVE and
                load XCR0 with x87 and SSE, and AVX where CPUID reports it and the
                standard-form area for them is 832 bytes with AVX at 576;
                load_extended_state_on_this_cpu shall load the same XCR0 on each
                secondary, whose CR4 is the boot processor's.";
            attribute :>> criterion = "The boot says 'program register state: x87,
                SSE and AVX, saved with XSAVE' on a model with AVX and XSAVEOPT, and
                'saved with FXSAVE; no XSAVE, so no AVX' on qemu64; Claude Code's
                AVX2-only runtime completes test-claude-code's turn on four
                processors, and on qemu64 does not.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::cpu::enable_extended_state",
                "arch::x86_64::cpu::load_extended_state_on_this_cpu",
                "arch::x86_64::cpu::write_xcr0");
        }
        requirement <'L.x86_64.122'> signalFrameCarriesExtendedState : ItemLowLevel {
            attribute :>> statement = "Where AVX is enabled, setup_signal_frame shall
                carry the interrupted state as Linux's XSAVE area -- FP_XSTATE_MAGIC1,
                the sizes and components in the software-reserved bytes, XSTATE_BV,
                the YMM upper halves, FP_XSTATE_MAGIC2 -- with UC_FP_XSTATE in
                uc_flags; restore_signal_frame shall take AVX back only from a whole
                area and reset it otherwise, take nothing of the header but
                XSTATE_BV, and XSTATE_BV shall name only AVX, x87 and SSE, and only
                XCR0's components, before XRSTOR64 sees it.";
            attribute :>> criterion = "A handler that sets every bit of its frame's
                XSTATE_BV, XCOMP_BV and reserved header bytes, keeping the magic
                words and sizes, returns through rt_sigreturn and its program runs
                on to exit 79 (the `xstate` line); with restore_fpu's and
                set_xstate_bv's masks both taken out, the boot stops on a #GP in
                ring 0, under TCG and under KVM.";
            attribute :>> parent = "H.TRAP.9";
            attribute :>> unit = ("arch::x86_64::signal::uc_flags",
                "arch::x86_64::signal::fp_area",
                "arch::x86_64::signal::fp_area_bytes",
                "arch::x86_64::signal::xstate_in",
                "arch::x86_64::signal::restore_fpu",
                "arch::x86_64::switch::UserState::set_xstate_bv");
        }
        requirement <'L.x86_64.123'> avxWithheldWhereVectorRegistersLeak : ItemLowLevel {
            attribute :>> statement = "With the speculation switch on, vector_leak
                shall, on a Zen 2 processor (family 0x17, Linux's amd_zenbleed
                models), set DE_CFG[9] on every processor unless the microcode is
                at the fixed revision, and withhold AVX from XCR0 in a guest; and
                on an Intel model Linux lists for Gather Data Sampling without
                GDS_NO, keep AVX only where GDS_CTRL's microcode mitigation is on or
                can be turned back on, which it does on every processor; the boot
                line shall say which.";
            attribute :>> criterion = "decide_vector_leak gives each of twelve
                processors -- Zen 2 bare metal with old, fixed and unlisted
                microcode, a Zen 2 guest, one without AVX, a Zen 5, Kaby Lake
                without and with GDS_NO, with GDS_CTRL on, off and unlocked, off and
                locked, an Alder Lake -- its verdict, and family_and_model reads a
                Matisse and a Kaby Lake right; with the guest rule taken out, the
                check fails. The writes, DE_CFG[9] and clearing GDS_MITG_DIS, are
                not exercised on the gate's machines, which are not affected parts;
                in a guest the verdict rests on the model the hypervisor presents
                (SPECULATION.md section 9, AoU-11).";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::vector_leak",
                "arch::x86_64::speculation::decide_vector_leak",
                "arch::x86_64::speculation::zenbleed",
                "arch::x86_64::speculation::gds",
                "arch::x86_64::speculation::is_zen2",
                "arch::x86_64::speculation::is_gds_model",
                "arch::x86_64::speculation::zenbleed_fixed_at",
                "arch::x86_64::speculation::family_and_model",
                "arch::x86_64::speculation::vector_facts",
                "arch::x86_64::speculation::set_zenbleed_bit",
                "arch::x86_64::speculation::enable_gds_mitigation",
                "arch::x86_64::speculation::apply_vector_cover",
                "arch::x86_64::speculation::VectorLeak::describe");
        }
        requirement <'L.syscall.1'> nativeNumbersReachTheNativeAbi : ItemLowLevel {
            attribute :>> statement = "dispatch_with shall send a native-entry
                number in the native range to native_call, which answers it for the
                calling task's own process.";
            attribute :>> criterion = "Two programs whose every native call is made
                by SYSCALL -- vmo_create, channel_write with a handle,
                object_wait_one, channel_read, vmo_read -- each exit 0.";
            attribute :>> parent = "H.OBJ.1";
            attribute :>> unit = ("syscall::dispatch_with", "syscall::native_call");
        }
        requirement <'L.syscall.2'> compatNeverReachesNative : ItemLowLevel {
            attribute :>> statement = "dispatch_with shall answer an int $0x80
                number in the native range with -ENOSYS, never as a native call.";
            attribute :>> criterion = "An int $0x80 with a number in 0x1000..0x1FFF
                answers -38.";
            attribute :>> parent = "H.TRAP.12";
            attribute :>> unit = "syscall::dispatch_with";
        }
        requirement <'L.syscall.3'> enosysIsReportedWithinItsBound : ItemLowLevel {
            attribute :>> statement = "unanswered shall print a call answered ENOSYS
                by name while report_unanswered's bound lasts, and nothing once it
                is spent.";
            attribute :>> criterion = "After report_unanswered(1), two unanswered
                calls print exactly one `syscall ... answered ENOSYS` line.";
            attribute :>> parent = "H.TRAP.11";
            attribute :>> unit = "syscall::unanswered";
        }
        requirement <'L.syscall.20'> everyEndGoesThroughTheCore : ItemLowLevel {
            attribute :>> statement = "The personality, an interface the item states:
                its Process::end, the one path exit_group, a kill, a job's kill, an OOM
                kill and a last thread's exit take, shall record the end through the
                core's end_record; and a task it launches after its process began to
                end, or while another thread replaces the program, shall be posted END
                through notify once it is listed, unless has_end says it holds it.";
            attribute :>> criterion = "As L.sched.35's: the OOM-killed program ends.";
            attribute :>> parent = ("H.OBJ.9", "H.QUOTA.6");
            attribute :>> unit = ("object::process::Process::end_record",
                "sched::work::has_end");
        }
        requirement <'L.syscall.21'> aReplacingThreadPostsEnd : ItemLowLevel {
            attribute :>> statement = "The personality, an interface the item states:
                execve's end_other_threads shall post END through post_to_tasks to
                every task of the process but the caller's after it records the
                replacing thread, waking each and interrupting its processor.";
            attribute :>> criterion = "In a program of three threads, two counting in
                user mode and one waiting, the first thread's replacement of the
                program sees the other two leave within a second (stage 7's
                replacement check); with the post removed the counting one outlives
                it and the check fails by name.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = "object::process::Process::post_to_tasks";
        }
        requirement <'L.syscall.22'> aStopPostsStop : ItemLowLevel {
            attribute :>> statement = "The personality, an interface the item states:
                enter_stop shall post STOP to every task of the process after it
                stores the stop: the caller's own through post_own when it is one of
                the process's threads, and every other's through post_to_tasks, woken
                and interrupted.";
            attribute :>> criterion = "The stop check's second stop, made with no
                signal, parks both counting threads; with the post to the other tasks
                removed it fails by name, and with the look of L.sched.33 on it stops
                with FX-0520.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("object::process::Process::post_to_tasks",
                "sched::work::post_own");
        }
        requirement <'L.syscall.23'> everySignalPostsSignal : ItemLowLevel {
            attribute :>> statement = "The personality, an interface the item states:
                SIGNAL shall be posted wherever needs_attention can turn true for a
                thread's signals: by Process::post with every
                signal it records, to the thread it was sent to or to every thread
                that does not block it; by change_blocked when a change unblocks a
                signal or saves a mask, to that thread; by force and by a marked
                restart, to the running task; to the thread a signal is handed on to;
                to a task launched with a signal it can take already pending; and
                by return_to_user to its own task when a pass ends on its delivery
                cap of 65, so the masked look goes round again for the rest.";
            attribute :>> criterion = "A signal sent to a process whose first thread
                blocks it is taken by its second, which spins alone on its processor
                (stage 7's hand-off check); with Process::post's and the wake's
                SIGNAL removed it never is, and the check fails by name. 35 ignored
                signals pending for a check's process and for its thread, 70
                deliveries, are all taken before the way back returns (the `capped`
                line); with the capped pass's post removed the boot stops on
                FX-0520.";
            attribute :>> parent = ("H.TRAP.4", "H.TRAP.9");
            attribute :>> unit = ("sched::work::post", "sched::work::post_own",
                "sched::work::notify");
        }
        requirement <'L.trap.5'> noEntryMeansEnosys : ItemLowLevel {
            attribute :>> statement = "system_call shall answer ENOSYS with no entry
                registered, and the first registration shall stand.";
            attribute :>> criterion = "With no entry registered, system_call answers
                -38; a second set_syscall_entry leaves the first in place.";
            attribute :>> parent = "H.TRAP.11";
            attribute :>> unit = ("trap::system_call", "trap::set_syscall_entry");
        }
        requirement <'L.trap.6'> aMovedProcessRunsInItsNewJob : ItemLowLevel {
            attribute :>> statement = "system_call and dispatch shall regroup the
                running task after a call or a trap from user mode, so that a
                process moved to another job runs in it from then on.";
            attribute :>> criterion = "After a program writes its pid to another
                cgroup's cgroup.procs, its next charges land in that job.";
            attribute :>> parent = "H.OBJ.12";
            attribute :>> unit = ("trap::system_call", "trap::dispatch");
        }
        requirement <'L.trap.7'> theRegisteredFilterIsAskedFirst : ItemLowLevel {
            attribute :>> statement = "filter_system_call shall answer nothing with no
                filter registered and ask the registered filter otherwise, the first
                registration shall stand, and an errno the filter answers shall be cut
                to 4095 before it is written as the call's result.";
            attribute :>> criterion = "Every entry of the running architecture, driven
                with frames of the check's own, asks the registered filter exactly
                once per call, and the errnos 0, 13, 512, 4095, 4096, 5000 and
                4294967295 come back as 0, -13, -512, -4095, -4095, -4095 and -4095;
                a slot with nothing registered answers nothing, and one registered
                twice asks the first (the `seccomp` line).";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = ("trap::filter_system_call", "trap::set_syscall_filter",
                "trap::register", "trap::ask");
        }
    }

    package InterruptRemapping {
        doc /* NVIDIA's N0g on x86-64 (docs/NVIDIA.md section 12.3): remapped
             * messages, the console's I/O APIC line converted at bring-up, and
             * x2APIC handled on every processor. */

        requirement <'L.x86_64.129'> messagesAreRemappable : ItemLowLevel {
            attribute :>> statement = "Where a unit remaps, msi_allocate shall
                write the vector's interrupt remapping entry, indexed by the
                vector's slot among the sixty-four, for the function's requester
                ID and the asking processor, and answer a remappable-format message
                naming it with data 0; where no unit remaps, a compatibility-format
                message as before. The check vector 0xFC shall be outside the
                sixty-four, only counted and acknowledged, and counted stray outside
                checks R1 and R2.";
            attribute :>> criterion = "On x86-64 under KVM and TCG edu's remapped
                MSI is delivered unmasked and not masked (R4) and virtio-rng's
                MSI-X completes; edu's MSI capability programmed in compatibility
                format to 0xFC (R1), and as an NMI (R2), faults 0x25 and arrives
                nowhere, and programmed with another function's entry (R3) faults
                0x26 and arrives nowhere within the unit's patience; with remapping left off the forged 0xFC
                arrives and R1 fails. Host test: a remappable message names its
                handle by bits 19:5 and 2, format bit 4.";
            attribute :>> parent = "H.DMA.9";
            attribute :>> unit = ("arch::x86_64::msi::msi_allocate",
                "iommu::remapping::route", "iommu::remapping::remap_message",
                "arch::x86_64::service_interrupts", "iommu::remapping::note_check_vector",
                "iommu::remapping::stray_deliveries");
        }
        requirement <'L.x86_64.130'> theConsoleLineIsConverted : ItemLowLevel {
            attribute :>> statement = "At a unit's interrupt remapping bring-up, the
                console's I/O APIC line, live in compatibility format since stage 3,
                shall be converted when that unit's DMAR scope names its I/O APIC by
                its MADT identifier: masked under its lock; a fresh vector V1 taken
                and its entry written, with the line's destination, trigger and the
                I/O APIC's source ID, outside the lock; V0 kept taken for good;
                and its handler moved to V1 through the generic interrupt layer,
                still masked, leaving V0 a handler that counts. No vector shall
                have been handed out by then but the line's, or the boot stops by
                name. After IRE, its entry shall be rewritten in remappable format
                with V1 as its vector, unmasked, and the port serviced once. With no scope naming its I/O APIC, no unit shall
                remap. A delivery on V0 afterwards shall be counted and fail the
                boot.";
            attribute :>> criterion = "On x86-64 under KVM (split irqchip) and TCG
                the bring-up line says the console's line is converted, V0 retired;
                check R9's line says every vector handed out is the console's; a
                byte looped back while the line is masked is read by the service
                once, by reading the port; check R5 sees a looped-back byte arrive
                through V1 and none on V0, and on a UART that raises no interrupt in
                loopback says it is not observable and fails only on a delivery to
                V0 (test-boot requires the positive line on the reference machine); the end-of-boot line counts 0 on V0. Controls: the line left
                unmasked through the conversion lets KVM's old compatibility route
                deliver on V0 after IRE, which is counted, and the boot fails; the entry left not present, the byte never
                arrives; the service dropped, the masked byte is not read; edu
                minted before bring-up, the boot stops on R9. Host tests: the
                remappable entry's handle and vector, and the I/O APIC's source ID
                matched by its identifier.";
            attribute :>> parent = ("H.DMA.9", "H.IRQ.1");
            attribute :>> unit = ("arch::x86_64::console_receive_irq",
                "arch::x86_64::route_console", "arch::x86_64::mask_console_line",
                "arch::x86_64::unmask_console_line", "arch::x86_64::convert_console_line",
                "arch::x86_64::retire_console_line", "arch::x86_64::restore_console_line",
                "irq::return_handler",
                "arch::x86_64::apic::IoApicInput::mask",
                "arch::x86_64::apic::IoApicInput::routed",
                "arch::x86_64::apic::IoApicInput::redirection",
                "arch::x86_64::apic::IoApicInput::write", "irq::move_handler",
                "iommu::remapping::note_hooks", "iommu::remapping::console_source",
                "iommu::remapping::prepare_console", "iommu::remapping::unmask_console",
                "iommu::remapping::finish_console",
                "iommu::remapping::require_no_compatibility_vector",
                "iommu::remapping::console_vectors");
        }
        requirement <'L.x86_64.131'> x2apicIsLeftOnEveryProcessor : ItemLowLevel {
            attribute :>> statement = "Every processor, the boot processor in
                apic::init and each application processor first in
                secondary_start, shall read its own IA32_APIC_BASE before it uses
                the local APIC's MMIO window, and one in x2APIC mode shall be taken
                to disabled and then to xAPIC; where the platform locks legacy xAPIC
                (IA32_XAPIC_DISABLE_STATUS), the boot shall stop naming the
                processor.";
            attribute :>> criterion = "On x86-64 under KVM and TCG the check puts
                the first application processor in x2APIC mode before its own look;
                every processor looks, that one is switched to xAPIC and comes up,
                and stage 4's apic line counts them; with secondary_start's look
                removed that processor never comes up and stage 4 fails.";
            attribute :>> parent = ("H.IRQ.1", "H.FAIL.1");
            attribute :>> unit = ("arch::x86_64::apic::leave_x2apic",
                "arch::x86_64::apic::xapic_locked", "arch::x86_64::apic::init",
                "arch::x86_64::smp::secondary_start", "arch::x86_64::smp::CpuStarter::finish");
        }
        requirement <'L.x86_64.132'> oneDestinationEncoder : ItemLowLevel {
            attribute :>> statement = "An interrupt remapping entry's destination
                shall be built only by ferrix_paging::vtd::remap::destination, in
                xAPIC format (the APIC ID in bits 15:8), and an APIC ID above 255
                shall be refused rather than truncated; the x2APIC work must set
                EIME and widen it.";
            attribute :>> criterion = "Host tests: IDs 0 and 255 encode in bits
                15:8, 256 and above are refused, and an entry carries the encoded
                destination in DST.";
            attribute :>> parent = "H.DMA.9";
            attribute :>> unit = ("iommu::remapping::remap_message",
                "iommu::remapping::prepare_console");
        }
    }
}
