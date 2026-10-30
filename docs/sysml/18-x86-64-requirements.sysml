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
                processor's TSS RSP0 -- which STR and SGDT find at each load of
                this processor's tables and its per-CPU record keeps
                (note_tables) -- and its SYSCALL entry stack at the incoming
                task's kernel-stack top.";
            attribute :>> criterion = "At every processor's bring-up, after its
                last note, its record's GDT and RSP0 addresses equal a fresh STR
                and SGDT answer and neither is zero; FX-0409 stops the machine
                otherwise.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::gdt::set_privilege_stack",
                "arch::x86_64::gdt::note_tables", "arch::x86_64::gdt::ask_privilege_stack",
                "arch::x86_64::gdt::task_register_loaded",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::cpu::read_task_register", "arch::x86_64::cpu::read_gdt");
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
                "arch::x86_64::gdt::live_table",
                "arch::x86_64::gdt::noted_table");
        }
        requirement <'L.x86_64.8'> threadAreaTravelsWithThread : ItemLowLevel {
            doc /* Restated for the DS/ES skip (OPAQUE-KERNEL.md §9.8, 3b; the
                consultant's S1-S7, ledger lines 393, 398 and 412): DS and ES are
                compared with the processor's own registers, read in the same
                restore, never with a remembered value. FS and GS are
                L.x86_64.165's (3c). */
            attribute :>> statement = "save_user_state shall save the outgoing
                thread's thread-local slots and data selectors, and
                restore_user_state shall write the incoming thread's slots and
                reload its selectors through loadable, except that DS and ES are
                each loaded from the record unless the selector the processor
                holds, read in the same restore, and the record's are both 0.";
            attribute :>> criterion = "Two i386 programs pinned to one processor,
                both with entry 12 and %gs 0x63 but bases 4 bytes apart, each read
                their own word through %gs 200 times with a sched_yield between
                reads: neither exits 46, both exit 42. Pinned beside a 64-bit
                program whose DS and ES are 0, one that leaves 3 (a null selector
                with RPL 3), one that leaves USER_DS, and an i386 program whose
                DS and ES are USER_DS: each reads its own DS and ES after each of
                1,000 yields; a 64-bit program with DS 0, beside one that loads a
                based thread-local descriptor and then 0 into DS and ES, ends by
                SIGSEGV when it reads through DS in compatibility mode under KVM
                or on hardware (under QEMU's TCG, which checks no data segment,
                the case is not decided and its line says so); and two
                64-bit programs with DS and ES 0 trading the processor count more
                than 0 switches that left them unloaded.";
            attribute :>> parent = "H.SCHED.7";
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::selector_skips",
                "arch::x86_64::switch::selector_skips_total",
                "arch::x86_64::cpu::read_data_selectors",
                "arch::x86_64::cpu::load_ds",
                "arch::x86_64::cpu::load_es",
                "arch::x86_64::cpu::load_fs",
                "arch::x86_64::cpu::load_user_gs");
        }
        requirement <'L.x86_64.9'> fsGsBaseTravelsWithTask : ItemLowLevel {
            doc /* The GS half of 3b (OPAQUE-KERNEL.md §9.8), in a row of its own.
                Until arch_prctl answered ARCH_SET_GS no call set a 64-bit
                program's GS base, every record was zero, and the row stood
                unverified (the consultant's condition (a), ledger 387). The call
                sets one now (docs/WINE.md §3), so two programs with different
                bases tell a written base from a kept one. The FS half is
                L.x86_64.61 and L.x86_64.158. */
            attribute :>> statement = "A task's program GS base shall be its
                UserState's record, written by arch_prctl(ARCH_SET_GS) beside the
                shadow MSR with interrupts masked, zeroed by execve beside the MSR
                and never read back at a switch, and restore_user_state shall
                write it at every switch to a task with user state whose GS
                selector is null.";
            attribute :>> criterion = "Two 64-bit programs pinned to one processor
                each set a distinct GS base with ARCH_SET_GS and read their own
                words through %gs 10,000 times across sched_yield (the `fsbase`
                line's GS half); with the call writing the MSR and not the record,
                or with the switch's write of the base removed, a read after a
                switch faults or reads the other program's word and the check
                fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::switch::save_user_state",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::UserState::set_bases",
                "arch::x86_64::switch::UserState::set_gs_base",
                "arch::x86_64::syscall::arch_prctl");
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
            attribute :>> criterion = "A fork child reads back the GS and FS bases
                its parent set with arch_prctl, the thread-local slot 12 it took
                with set_thread_area, user data in DS, MXCSR 0x3F80, x87 control
                word 0x027F and an XMM0 pattern; a clone thread reads both bases
                (the `inherit` line). With capture's GS base made 0 the check fails
                by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = "arch::x86_64::switch::UserState::capture";
        }
        requirement <'L.x86_64.12'> newProgramStartsWithResetState : ItemLowLevel {
            attribute :>> statement = "reset_user_state shall, at execve, empty the
                thread-local slots, load null data selectors with zero FS and GS
                bases, and load UserState::new's x87 control word 0x037F and MXCSR
                0x1F80.";
            attribute :>> criterion = "A program the kernel started reads a GS base
                of 0; after it set both bases, slot 12, DS, MXCSR 0x3F80 and control
                word 0x027F and execve'd itself, the new image finds GS and FS bases
                0, get_thread_area(12) empty, DS 0, MXCSR 0x1F80 and x87 control
                word 0x037F (the `inherit` line). With the reset's GS base left as
                it was the check fails by name.";
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
                unsaved, put every component XCR0 enables less PKRU in its initial
                state, with the task's own MXCSR and x87 control word, leaving XINUSE
                independent of what ran before, and clear the mark; nothing else
                shall clear it.";
            attribute :>> criterion = "The wakes of the `vectors` line read neither
                program's pattern and their own MXCSR and control word, beside a
                program that used the x87 and one that left it initial, with both of
                the fast reset's x87 branches taken where XGETBV 1 is offered and the
                probe, woken after a quiet program, reads the x87's XINUSE bit clear
                in the first pair and in each of three more probes (set may mean
                initial, SDM Vol. 1 13.6), the x87-left branch within 16 wakes or counted on any processor in the boot, and a probe woken after a
                busy and after a quiet program reads no XINUSE bit set after the busy
                one and clear after the quiet one, in one of up to three pairs; with
                the x87 reset done by FNINIT, VZEROALL, the x87 reset or LDMXCSR removed, or the XINUSE
                test inverted, the check fails by name.";
            attribute :>> parent = "H.SCHED.12";
            attribute :>> unit = ("arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::reset_vectors", "arch::x86_64::cpu::load_x87_control",
                "arch::x86_64::cpu::zero_vectors", "arch::x86_64::cpu::x87_in_use",
                "arch::x86_64::switch::count", "arch::x86_64::switch::this_logical");
        }
        requirement <'L.x86_64.156'> anUnsavedStateReadsAsInitial : ItemLowLevel {
            attribute :>> statement = "Every reader of a task's saved vector area
                shall read it through UserState::vectors, which shall answer an
                unsaved state as the initial state with the task's own MXCSR and x87
                control word, except two that never meet an unsaved state:
                restore_user_state's full restore, which runs only where the state
                is not unsaved, and load_fpu, whose state rt_sigreturn captured in
                full; a writer shall first make it that state.";
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
                task's recorded FS base where its FS selector is null at every
                switch to a task with user state, never skipping a write equal to
                what the processor last held, and save_user_state shall read no FS
                base MSR.";
            attribute :>> criterion = "Two programs with different FS bases trade one
                processor 10,000 times each, each reading its own word (the `fsbase`
                line); a program that loads USER_DS and then a null selector into FS
                before each yield has its record equal to the base its processor
                last wrote while the base it holds is zero, and a second whose
                recorded base equals that record reads its own word at every turn; with the write skipped where it equals the last one written,
                the second ends with SIGSEGV and the check fails by name.";
            attribute :>> parent = "H.SCHED.8";
            attribute :>> unit = ("arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::save_user_state");
        }
        requirement <'L.x86_64.165'> fsAndGsSkippedZeroOverZero : ItemLowLevel {
            doc /* The FS/GS skip (OPAQUE-KERNEL.md §9.8, 3c; the consultant's
                design verdict, ledger line 445, K1-K10, after lines 393, 398
                and 412). Split from L.x86_64.8 (K6). */
            attribute :>> statement = "restore_user_state shall load FS and GS
                from the record unless the selector the processor holds, read in
                the same restore, and the record's are both exactly 0; shall
                write the FS and GS bases from the record wherever the record's
                selector is 0, whether the load was skipped or not; and every
                processor shall load the null selector into FS and GS before it
                writes its per-CPU GS base.";
            attribute :>> criterion = "Pinned beside a 64-bit reader whose FS
                and GS are 0 and whose FS base is its own page, a program that
                leaves USER_DS, one that leaves 3, one that loads a based
                thread-local descriptor and then 0 into FS and GS, and an i386
                program with a thread-local selector in GS and FS: the reader
                reads FS and GS 0, its own %fs:0 word and its GS base of 0
                (%gs:0x400100 equal to its own word there) after each of 1,000
                yields, and the i386 program its own selector and %gs:0 and %fs:0
                words; a 64-bit program with FS (GS) 0 that far-returns into
                compatibility mode and reads through it ends by SIGSEGV under KVM
                or on hardware, beside the descriptor program and on an
                application processor before any stage 9 case runs there (under
                QEMU's TCG, which
                checks no data segment, not decided and the fsgs line says so);
                and two readers trading the processor count more than 0 switches
                that left FS unloaded and more than 0 that left GS unloaded.";
            attribute :>> parent = ("H.SCHED.7", "H.SCHED.8");
            attribute :>> unit = ("arch::x86_64::switch::load_selectors",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::switch::fs_gs_skips",
                "arch::x86_64::switch::selector_skips_total",
                "arch::x86_64::cpu::load_null_fs_gs",
                "arch::x86_64::set_cpu_local");
        }
        requirement <'L.x86_64.159'> theFastPathIsEnteredFromSyscallOnly : ItemLowLevel {
            attribute :>> statement = "ferrix_syscall_entry shall call the fast path
                only for 0x1013 and only while the core's fast-path flag is raised,
                read at every call, which fastpath::init does for ferrix.fastpath=on
                alone; before the filter and with interrupts masked, after every
                entry measure the stub makes; raising the vector mark first and
                lowering it on a decline.";
            attribute :>> criterion = "Booted with the fast path off, no counter
                moves; on, trips are taken (test-ipc-equiv).";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("trap::fast_write_read", "trap::fast_path_on",
                "trap::fast_write_read_now");
        }
        requirement <'L.x86_64.161'> theCoreCallsUpOnlyThroughListedHooks : ItemLowLevel {
            attribute :>> statement = "The core shall reach the fast path and T2's
                quiet predicate only through the two link-time hooks
                composition_root.hooks names, each declared once in trap.rs, defined
                once in main.rs as one call of the function the entry names, and
                tied on both sides to one fn alias; and shall make each call only
                while a core flag read at that call says so: the fast path while
                the fast-path flag is raised, the predicate only for a filter
                registered with it by trap::set_quiet_syscall_filter, which raises
                that flag after the filter is in place and only for the filter it
                installed.";
            attribute :>> criterion = "check-item-boundary.py refuses an
                extern-Rust declaration in the core that the list does not name, a
                hook defined outside main.rs and one whose body does more than
                forward (its self-test cases, each made on the tree as well); booted
                with ferrix.fastpath=off no fast path counter moves and with on
                trips are taken, and stage 9's case 11 is refused through the
                predicate on both boots (`cargo xtask test-ipc-equiv`).";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("trap::filter_quiet", "trap::set_quiet_syscall_filter",
                "trap::fast_write_read_now");
        }
        requirement <'L.x86_64.160'> theFrameTailWritesFourRegisters : ItemLowLevel {
            attribute :>> statement = "frame_tail shall write the reply into RAX, RSI,
                RDX and R10 and no other register, and leave by the stub's exit only
                with nothing due and the filter quiet, otherwise opening interrupts,
                stopping with FX-0533 where the task may not block, and running
                regroup_current, call_left and the way out.";
            attribute :>> criterion = "ipc-equiv's cases 1 and 2 with the fast path
                on; case 14 and the continuation's kill leave by the general branch.";
            attribute :>> parent = "H.OBJ.18";
            attribute :>> unit = ("arch::x86_64::syscall::frame_tail",
                "arch::x86_64::syscall::write_outcome", "arch::x86_64::syscall::leave");
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
                taken in the kernel or in a program and return from it, with
                note_gs finding its GS naming a processor's record whatever GS
                base the program holds, and send and issue shall deliver the NMI
                a processor sends to its own APIC ID or to another's.";
            attribute :>> criterion = "An NMI sent to this processor with interrupts
                masked is accepted, at least one NMI is counted and returned from,
                and 0 handler runs find a GS naming no processor's record (the `nmi`
                line). At least 3 NMIs sent to another processor are taken in ring
                3 of a program there whose GS base is 0x7ffffffff000, 0 with a
                foreign GS, and the program exits with its status (the second
                `debug` line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::paranoid::ferrix_paranoid_entry",
                "arch::x86_64::paranoid::note_gs",
                "arch::x86_64::apic::id",
                "arch::x86_64::apic::send_nmi_to");
        }
        requirement <'L.x86_64.25'> syscallWindowBreakpointsSurvived : ItemLowLevel {
            attribute :>> statement = "debug shall count a kernel #DB against the
                breakpoint slot DR6 names, set RF so that the instruction runs, and
                return, with note_gs finding the kernel's GS, including in the
                SYSCALL trampoline's stretches on the program's stack.";
            attribute :>> criterion = "Breakpoints on the SYSCALL stub's swapgs and
                on its sysretq, while a program writes and exits, fire at least 2
                and at least 1 times, 0 of them with a foreign GS, and the program
                exits with its status (the first `debug` line); and again beside
                a program that first sets its GS base to 0x7ffffffff000, whose
                upper half the entry's sign test then reads (the second `debug`
                line).";
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
                build, issue no IBPB, and refill the return stack with 32 entries
                unless CPUID 0x8000_0021 EAX bit 24 (ERAPS) is set and CR4.PCIDE is
                clear.";
            attribute :>> criterion = "Switches between two members of one marked
                job decide no barrier (the `domain` line, case 1), and the hardened
                build's return-stack refill runs at each of them, or at none of them
                where CPUID, read by the check itself, says ERAPS with CR4.PCIDE
                clear.";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = ("arch::x86_64::speculation::switch_barrier_in_domain",
                "arch::x86_64::speculation::fill_return_stack",
                "arch::x86_64::speculation::eraps_empties_on_switch");
        }
        requirement <'L.x86_64.127'> patEntryOneWriteCombining : ItemLowLevel {
            attribute :>> statement = "Every processor shall program IA32_PAT with
                X86_PAT before anything maps write-combining, entry 1 write-combining
                and entries 0 and 3 write-back and uncacheable as at power-on: the
                boot processor before the kernel maps its framebuffer window, and
                every secondary in its trampoline before it turns paging on, when the
                boot processor's table read back as X86_PAT; and a leaf the kernel
                maps write-combining shall select entry 1 by its write-through bit
                alone, with device memory's uncacheable entry winning where both are
                asked.";
            attribute :>> criterion = "Host tests: a write-combining leaf has PWT
                and not PCD, reads back as itself, and loses to device; X86_PAT's
                entries 0, 1 and 3 are 0x06, 0x01 and 0x00. On x86-64 the boot's
                cpu line says entry 1 is write-combining, every processor read its
                PAT back, and every secondary read IA32_PAT as X86_PAT on arriving
                in secondary_start, before programming it (counted equal to the
                secondaries, the fbwindow line), or stage 9's check fails; with the
                trampoline's write removed, it fails.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::x86_64::cpu::program_pat",
                "arch::x86_64::program_memory_types",
                "arch::x86_64::write_combining_processors", "arch::x86_64::init_traps",
                "arch::x86_64::smp::CpuStarter::new",
                "arch::x86_64::smp::secondary_start");
        }
        requirement <'L.x86_64.176'> theFramebufferWindowCombinesFromItsFirstMap : ItemLowLevel {
            attribute :>> statement = "On x86-64 the kernel shall map its framebuffer
                window by leaves selecting PAT entry 1 only when the boot processor's
                IA32_PAT reads back X86_PAT before the window is mapped and the
                processor has CLFLUSH, and as device memory otherwise; and it shall
                flush every cache line of the window before the window's first
                store.";
            attribute :>> criterion = "Host test: MapFlags::x86_framebuffer gives
                KERNEL_COMBINING for X86_PAT and KERNEL_DEVICE for the power-on table,
                0 and none read. On every x86-64 boot with a framebuffer, stage 9
                reads every page of the window back write-combining, not device,
                writable, not user's and global, the PAT recorded at the map as
                X86_PAT, and the lend's read-back true on the window and false on a
                device page (the fbwindow line, with the lines flushed); with the
                window mapped as device memory, the table programmed after the map,
                the read-back answering true for any page, or the flush not made
                (the count of lines flushed, checked equal to the window's), the
                boot panics. That the lines left the caches is build evidence: no
                emulator keeps a line for it to find. Without CLFLUSH the window's
                PAT is not recorded and the check fails on it: closed.";
            attribute :>> parent = ("H.MEM.15", "H.MEM.1");
            attribute :>> unit = ("arch::x86_64::framebuffer::framebuffer_flags",
                "arch::x86_64::framebuffer::window_pat",
                "arch::x86_64::framebuffer::flush_window",
                "arch::x86_64::framebuffer::window_flush",
                "arch::x86_64::framebuffer::window_lines",
                "arch::x86_64::framebuffer::line_size",
                "early::EarlyMemory::map_framebuffer");
        }
        requirement <'L.x86_64.177'> hardwarePStatesAskedForTheirWholeRange : ItemLowLevel {
            attribute :>> statement = "Every x86-64 processor whose CPUID.06H:EAX
                bit 7 is set shall, at its bring-up and after its IDT is loaded, set
                IA32_PM_ENABLE bit 0 where it reads clear and, where its own
                IA32_HWP_CAPABILITIES report a Highest_Performance above 0 and not
                below Lowest_Performance, write IA32_HWP_REQUEST with
                Minimum_Performance the Lowest and Maximum_Performance the Highest,
                Desired_Performance and the Activity_Window 0, Package_Control
                clear, and an Energy_Performance_Preference of the preference
                L.x86_64.182 chose where CPUID.06H:EAX bit 10 is set and 0
                otherwise. It shall count itself
                as asked only when IA32_PM_ENABLE bit 0 and IA32_HWP_REQUEST read
                back as written, and as left to firmware otherwise, which the boot
                processor's cpu line says and which does not stop the boot. Once
                the secondaries are up, the processors counted as asked and as left
                to firmware shall together be every processor online where the
                processor checking has CPUID.06H:EAX bit 7, and none shall be
                counted where it has not.";
            attribute :>> criterion = "Stage 3 composes the request from made-up
                capabilities (an N150-like core, levels 1 to 36, with and without a
                preference, and with the reserved upper half set) and, on the
                processor running stage 3, finds nothing counted where it has no
                HWP; stage 9 compares the counts with smp::count and prints the hwp
                line. Any other answer panics the boot. Negative controls: the
                request's Max field dropped, and the count taken above the CPUID
                gate, each panic under QEMU; the secondaries' call removed panics
                only on HWP hardware (the N150 box). QEMU exposes no HWP, so the
                written path, its read-back and the count of 4 are hardware
                evidence: the N150 boot's cpu and hwp lines.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::x86_64::cpu::hwp_request",
                "arch::x86_64::cpu::has_hwp",
                "arch::x86_64::cpu::enable_hwp",
                "arch::x86_64::cpu::hwp_processors",
                "arch::x86_64::cpu::hwp_refused",
                "arch::x86_64::enable_hwp_and_say",
                "arch::x86_64::smp::secondary_start");
        }
        requirement <'L.x86_64.182'> hardwarePStatePreferenceChosenByOption : ItemLowLevel {
            attribute :>> statement = "The x86-64 kernel shall read the boot option
                ferrix.epp, on the boot processor, from the loader's command
                line, with option_in's grammar (the first ferrix.epp= word; a bare
                ferrix.epp is no option), and choose the energy and performance
                preference: 0 where there is no option and for performance, 0x80
                for balance_performance, 0xc0 for balance_power, 0xff for power,
                the number for one to three ASCII decimal digits whose value is 0
                to 255, and 0 for any other value, which the boot processor's cpu
                line says is not understood. The preference so chosen shall be the
                one every processor's IA32_HWP_REQUEST uses (L.x86_64.177).";
            attribute :>> criterion = "Stage 3 runs the chooser over a table: no
                option, the four names, 0, 00, 007, 128 and 255 choose their
                values; 256, 999, 1000, 0255, -1, +1, 0x80, the empty value,
                Performance, a trailing or leading space and default are refused
                and give 0. It then reads ferrix.epp from the command line again
                and requires the preference in force to be the chooser's answer
                for it. Any other answer panics the boot. Negative controls: the
                default made 0x80, the number arm wrapping 256 to a byte, and the
                read in kmain removed on a boot with ferrix.epp=power, each panic
                in stage 3. Boots with ferrix.epp=power and ferrix.epp=bogus print
                the cpu line for each.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::x86_64::cpu::choose_preference",
                "arch::x86_64::cpu::decimal_preference",
                "arch::x86_64::cpu::read_preference",
                "arch::x86_64::cpu::preference",
                "arch::x86_64::cpu::Preference::epp",
                "arch::x86_64::read_boot_options");
        }
        requirement <'L.x86_64.187'> msrLogOnlyWhenAsked : ItemLowLevel {
            attribute :>> statement = "The x86-64 kernel shall read the power-limit,
                limit-reason, performance and thermal MSRs of ferrix.msrlog (0x606,
                0x610, 0x614, 0x601, 0x65c, 0x64f, 0x6b0, 0x198, 0x1a2, 0x19c,
                0x1b1) only when the bare word ferrix.msrlog is on the loader's
                command line: then from one task pinned to the boot processor,
                started after the scheduler, once and again every
                ferrix.msrlog.period seconds (0 to 3600 of one to four ASCII
                digits, 0 for once; 5 without the option or for any other value,
                which a line says), each read through the checked read of
                L.x86_64.188, printing each raw value or absent and the decoded
                fields on lines beginning msr:. It shall write no MSR. Without
                the word it shall read no MSR, start no task and print no line.";
            attribute :>> criterion = "Stage 3 runs the period chooser over a table:
                no option, 0, 1, 0005 and 3600 choose their seconds; the empty
                value, 3601, 10000, -1, +1, 0x5, a leading space and 5s are
                refused. The gate's check row finds no wrmsr in msrlog.rs or its
                modules. A KVM boot with ferrix.msrlog and ferrix.msrlog.period=2
                prints msr: rounds 2 s apart with present and absent registers;
                the boots without the word print no msr: line.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::x86_64::msrlog::start",
                "arch::x86_64::msrlog::choose_period",
                "arch::x86_64::msrlog::intel",
                "arch::x86_64::msrlog::run",
                "arch::x86_64::msrlog::read",
                "arch::x86_64::msrlog::log_once",
                "arch::x86_64::msrlog::say_limit",
                "arch::x86_64::msrlog::say_reasons",
                "arch::x86_64::msrlog::ReasonNames::fmt",
                "arch::x86_64::start_msr_log");
        }
        requirement <'L.x86_64.188'> checkedMsrReadResumesAtItsFixup : ItemLowLevel {
            attribute :>> statement = "read_msr_checked shall return the value
                rdmsr reads for a register the processor implements and None for
                one it refuses with #GP: the trap entry shall resume a #GP taken
                from ring 0 whose saved instruction pointer is exactly the
                checked read's one rdmsr instruction at that read's fault label,
                which returns failure without reading EDX:EAX. The match shall be
                that one address, vector 13 and ring 0, with no range.";
            attribute :>> criterion = "Stage 3 on every x86-64 boot: the checked
                read of IA32_APIC_BASE equals rdmsr's with no #GP counted; the
                checked read of 0x3fffffff is None with exactly one #GP counted,
                or, where the machine answers unknown registers without #GP (TCG,
                KVM with ignore_msrs), a value with none counted; the cpu line
                says which. The x86_64:kvm gate row requires the #GP line.
                Negative control: the trap entry's rewrite of the instruction
                pointer removed, the x86_64:kvm boot stops in stage 3 with a
                general protection fault.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::x86_64::cpu::ferrix_rdmsr_checked",
                "arch::x86_64::cpu::read_msr_checked",
                "arch::x86_64::cpu::checked_read_resume",
                "arch::x86_64::cpu::checked_reads_refused",
                "arch::x86_64::trap::ferrix_trap_entry");
        }
        requirement <'L.x86_64.189'> msrLogDecodesIntelLayouts : ItemLowLevel {
            attribute :>> statement = "ferrix.msrlog's decoder shall take, from raw
                values: the RAPL power, energy and time units (0x606 bits 3:0,
                12:8, 19:16); PL1 and PL2 of 0x610 (15-bit power, enable, clamp,
                7-bit window, from bits 0 and 32; lock bit 63) and of 0x65c (17-bit
                power fields); the VR current limit of 0x601 (bits 12:0 in 1/8 A,
                as milliamperes; lock bit 31); TDP, minimum
                and maximum of 0x614; the limit reasons now and logged (bits 15:0,
                31:16) with turbostat's names; the ratio and voltage of 0x198; TjMax
                of 0x1a2; and the status, log, readout and valid bit of 0x19c and
                0x1b1; and convert power units to milliwatts and windows to
                milliseconds, rounding down.";
            attribute :>> criterion = "Stage 3 decodes fixed values of each
                register and compares every field; any difference panics the
                boot.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::x86_64::msrlog::decode::Units::from_raw",
                "arch::x86_64::msrlog::decode::Units::milliwatts",
                "arch::x86_64::msrlog::decode::Units::window_millis",
                "arch::x86_64::msrlog::decode::bit",
                "arch::x86_64::msrlog::decode::limit",
                "arch::x86_64::msrlog::decode::package_limits",
                "arch::x86_64::msrlog::decode::platform_limits",
                "arch::x86_64::msrlog::decode::vr_current",
                "arch::x86_64::msrlog::decode::power_info",
                "arch::x86_64::msrlog::decode::reasons",
                "arch::x86_64::msrlog::decode::perf_status",
                "arch::x86_64::msrlog::decode::tjmax",
                "arch::x86_64::msrlog::decode::thermal");
        }
        requirement <'L.x86_64.192'> checkedMsrWriteResumesAtItsFixup : ItemLowLevel {
            attribute :>> statement = "write_msr_checked shall return true after a wrmsr the processor
                accepts and false for one it refuses with #GP: the trap entry
                shall resume a #GP taken from ring 0 whose saved instruction
                pointer is exactly the checked write's one wrmsr instruction at
                that write's fault label, which returns failure. The match shall
                be that one address, vector 13 and ring 0, with no range, beside
                the checked read's. Outside cpu.rs only the power core,
                powerctl.rs, shall call it, with 0x601, its boot check, with
                an absent index, and the machine check's bring-up, mce.rs, with
                IA32_MCG_CTL and the banks' registers (L.x86_64.232); inside
                cpu.rs nothing but its definition.";
            attribute :>> criterion = "Stage 3, under a hypervisor only: a checked write of 0x3fffffff is
                refused with exactly one #GP counted under KVM, which panics
                otherwise; under another hypervisor (TCG) the line says which path
                ran; on bare metal it is not run. The gate's check row holds the
                call sites, the register and the single wrmsr (check-device-
                access.py), with controls planting a checked write in msrlog.rs,
                one of 0x1a0 in powerctl.rs and one in powerctl/fake_check.rs.
                Negative control: the write's address removed from the trap entry,
                the x86_64:kvm boot stops in stage 3 with a general protection
                fault.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::x86_64::cpu::ferrix_wrmsr_checked",
                "arch::x86_64::cpu::write_msr_checked",
                "arch::x86_64::cpu::checked_write_resume",
                "arch::x86_64::cpu::checked_writes_refused",
                "arch::x86_64::trap::ferrix_trap_entry");
        }
        requirement <'L.x86_64.194'> powerModelGateAndOnePackage : ItemLowLevel {
            attribute :>> statement = "logic::row shall name a row of the power core's model table only
                for a GenuineIntel processor whose CPUID.1:EAX family and model,
                extended model included, are a row's: family 6 model 0xBE alone;
                logic::packages shall count the processors' packages by their
                x2APIC ids shifted by the package shift, and the probe shall
                refuse unless it is exactly one.";
            attribute :>> criterion = "Stage 3: Intel 6/0xBE passes; AMD 6/0xBE, Intel 6/0xBF, 6/0xAE,
                15/0xBE and the gate host's 0x60fb1 do not; four ids under one
                shift are one package, two packages are two, none is none.
                Negative control: the gate's vendor test removed panics stage 3.";
            attribute :>> parent = "H.DEV.5";
            attribute :>> unit = ("arch::x86_64::powerctl::logic::row",
                "arch::x86_64::powerctl::logic::packages",
                "arch::x86_64::powerctl::logic::refusal");
        }
        requirement <'L.x86_64.195'> powerSetWithinTheRow : ItemLowLevel {
            attribute :>> statement = "power_set shall refuse with INVALID_ARGS and write nothing an
                eighths value below firmware's own 0x601 bits 12:0 or above the
                lower of the row's ceiling and firmware's plus the row's raise;
                for the N150's row the ceiling is 48 A and the raise 23 A.";
            attribute :>> criterion = "Stage 3: over firmware's 25 A, 25 and 48 A are admitted, 24.875
                and 48.125 A refused; the highest over 31.25 A is 48 A, over 12.5
                A 35.5 A. Negative control: the ceiling removed panics stage 3.";
            attribute :>> parent = "H.DEV.5";
            attribute :>> unit = ("arch::x86_64::powerctl::logic::admitted",
                "arch::x86_64::powerctl::logic::highest",
                "arch::x86_64::powerctl::State::set",
                "arch::x86_64::powerctl::State::admit",
                "arch::x86_64::powerctl::set",
                "arch::x86_64::power_set",
                "arch::x86_64::power_status");
        }
        requirement <'L.x86_64.196'> powerWriteChangesOnlyBitsTwelveToZero : ItemLowLevel {
            attribute :>> statement = "A power write shall write 0x601 as the register read just before
                it with bits 12:0 replaced by the eighths asked and every other
                bit as read.";
            attribute :>> criterion = "Stage 3 composes over a word with every other bit set and compares
                the whole word; a field past 13 bits writes nothing outside it.
                Negative control: compose not masking panics stage 3.";
            attribute :>> parent = "H.DEV.5";
            attribute :>> unit = ("arch::x86_64::powerctl::logic::compose",
                "arch::x86_64::powerctl::write",
                "arch::x86_64::powerctl::Msr::write");
        }
        requirement <'L.x86_64.197'> powerReadIsReadOnlyAndMarksAbsence : ItemLowLevel {
            attribute :>> statement = "power_read shall write POWER_READ_WORDS words: firmware's 0x601,
                the ceiling, the signature, the latch flags, 0x601 as it reads,
                and the fixed read-only list 0x1a2, 0x19c, 0x1b1, 0x198, 0x64f,
                0x6b0, 0x611, 0x606, 0x610, each marked present in word 0 only if
                its read did not fault, and shall renew the claim's lease; it
                shall write no register.";
            attribute :>> criterion = "Stage 3 on the fake: every word read and marked; with the thermal
                readings faulting, their bits clear.";
            attribute :>> parent = "H.DEV.5";
            attribute :>> unit = ("arch::x86_64::powerctl::read",
                "arch::x86_64::powerctl::readings",
                "arch::x86_64::powerctl::Msr::reading",
                "arch::x86_64::powerctl::held",
                "arch::x86_64::powerctl::State::renew",
                "arch::x86_64::power_read");
        }
        requirement <'L.x86_64.198'> powerRestoreSingleShotOnHolderGone : ItemLowLevel {
            attribute :>> statement = "A restore shall write firmware's 0x601 bits 12:0 back only if a
                value the core wrote may be in force, taking it out of force so
                that of every caller only one writes, and the watch task shall
                restore and end the claim within a second of the holder's end
                closing; a second claim shall be refused while one stands; the
                kernel's end of a claim shall be the claim's own, dropped only
                after it ends and outside the core's lock, and a watch shall look
                only at the claim it was started for.";
            attribute :>> criterion = "Stage 3 on the fake: a raise is written, firmware's value is set
                again, the holder gone restores 25 A and does not latch, a second
                restore finds nothing in force, a second claim is refused; a claim
                ended and a new one made, the first claim's watch serves no more
                and the new claim's end is kept. Negative controls: the holder's
                death not restoring, and the watch serving any claim, each panic
                stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = ("arch::x86_64::powerctl::Restorer::new",
                "arch::x86_64::powerctl::Restorer::step",
                "arch::x86_64::powerctl::Restorer::run",
                "arch::x86_64::powerctl::Restore::confirmed",
                "arch::x86_64::powerctl::State::claim",
                "arch::x86_64::powerctl::State::tick",
                "arch::x86_64::powerctl::State::restore_once",
                "arch::x86_64::powerctl::watch",
                "arch::x86_64::powerctl::State::serves",
                "arch::x86_64::powerctl::State::release",
                "arch::x86_64::powerctl::State::gone",
                "arch::x86_64::powerctl::unserved");
        }
        requirement <'L.x86_64.199'> powerLeaseLapses : ItemLowLevel {
            attribute :>> statement = "The power core shall end a claim and restore firmware's 0x601 when
                no power_set or power_read was made on it for 5 s, looked at once
                a second; a claim so ended shall not latch the core.";
            attribute :>> criterion = "Stage 3 on the fake: a claim renewed at 4 s holds at 8 s and is
                restored at 9 s, and a new claim follows. Negative control: the
                lease never lapsing panics stage 3. cargo xtask test-power:
                restored on Lease after the driver stops renewing.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = "arch::x86_64::powerctl::logic::lapsed";
        }
        requirement <'L.x86_64.200'> powerBackstopRestoresAtNinety : ItemLowLevel {
            attribute :>> statement = "While a raise is in force the watch task shall read 0x1a2, 0x19c
                and 0x1b1 once a second and end the claim and restore when a read
                faults, 0x19c's valid bit 31 is clear, any of bits 0 to 5 is set
                in either status, or TjMax less the nearer readout is 90 C or
                more; power_set shall refuse a raise with LIMIT_REACHED while that
                verdict is bad, and not a lowering.";
            attribute :>> criterion = "Stage 3: 89 C passes, 90 C on either sensor trips, each of bits 0
                to 5 trips on either register and 6 to 15 do not, an invalid
                readout trips; on the fake, 90 C and a faulting read restore, a
                raise under a set PROCHOT bit is refused and a lowering written.
                Negative control: the limit removed panics stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = ("arch::x86_64::powerctl::logic::verdict",
                "arch::x86_64::powerctl::State::backstop");
        }
        requirement <'L.x86_64.201'> powerLatchesOffUntilReboot : ItemLowLevel {
            attribute :>> statement = "A backstop trip, a write that faults, reads back otherwise or
                meets a stop, a restore not confirmed, and the eighth claim ended
                in a boot shall latch the power core off until reboot: every later
                claim refused with BAD_STATE. A holder gone or a lease lapsed
                alone shall not.";
            attribute :>> criterion = "Stage 3 on the fake: a trip, an unconfirmed restore and eight
                ended claims each latch; a latched core refuses a claim. Negative
                control: the latch not set panics stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = ("arch::x86_64::powerctl::State::end",
                "arch::x86_64::powerctl::Cause::latches");
        }
        requirement <'L.x86_64.202'> powerFirmwareChangeIsLeftAlone : ItemLowLevel {
            attribute :>> statement = "Firmware's 0x601 shall be read once, at the probe. A claim, a
                write and a restore shall each compare the register with the last
                value the core wrote, or firmware's before any write, and write
                nothing over a value that differs; the register shall then be left
                to firmware until reboot. A restore shall write firmware's bits
                12:0 over the register as it reads, every other bit kept.";
            attribute :>> criterion = "Stage 3 on the fake: a register changed before a claim, before a
                write and while in force is written no more; a restore keeps a bit
                set outside the field. Negative control: the write's compare
                removed panics stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = ("arch::x86_64::powerctl::write",
                "arch::x86_64::powerctl::State::claim");
        }
        requirement <'L.x86_64.203'> powerRaiseFloor : ItemLowLevel {
            attribute :>> statement = "power_set shall refuse with SHOULD_WAIT a raise less than a second
                after the last raise, whichever claim made it, and shall take a
                lowering at once.";
            attribute :>> criterion = "Stage 3 on the fake: a raise half a second after one is refused, a
                lowering then written, a raise a second after written; a new claim
                does not reset the floor. Negative controls: the floor removed,
                and the floor reset per claim, each panic stage 3.";
            attribute :>> parent = "H.DEV.5";
            attribute :>> unit = "arch::x86_64::powerctl::logic::raise_due";
        }
        requirement <'L.x86_64.204'> powerWriteAndRestoreInterleave : ItemLowLevel {
            attribute :>> statement = "A power write shall run with interrupts masked on its processor,
                and shall write firmware's bits 12:0 back itself when a power-off,
                reset or panic restore has stopped the core before its last check,
                so that no interleaving of a write's accesses with such a
                restore's steps leaves a raised value in force.";
            attribute :>> criterion = "Stage 3 runs every interleaving of a raise's 13 accesses with an
                exit restore's 6 steps, from firmware's value and from a raise in
                force, on the fake: every one ends with firmware's field and
                nothing in force. Negative control: the write's own revert removed
                panics stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = "arch::x86_64::powerctl::Msr::masked";
        }
        requirement <'L.x86_64.205'> powerWriteIsReadBack : ItemLowLevel {
            attribute :>> statement = "A power write shall read 0x601 back; a back whose bits 12:0 are
                not those asked, a write the processor refused with #GP, or a read
                that faulted shall end the claim, and the restore shall write
                firmware's bits 12:0 over the value read back.";
            attribute :>> criterion = "Stage 3 on the fake: a write read back with a bit flipped ends the
                claim with firmware's value in the register, and a faulted write
                ends it with firmware's value in the register. Negative control:
                the read-back value not taken as the last written panics stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = "arch::x86_64::powerctl::State::set";
        }
        requirement <'L.x86_64.206'> powerExitRestore : ItemLowLevel {
            attribute :>> statement = "restore shall, on an orderly power-off, a reset and both ends of a
                panic, from any processor, with no lock and no allocation, stop
                the power core and write firmware's 0x601 bits 12:0 back if a
                raise may be in force, and print a line saying so; no raise shall
                follow it.";
            attribute :>> criterion = "Stage 3 on the fake: after a raise the exit restore confirms
                firmware's value, and a write after it writes nothing. cargo xtask
                test-power's lease row run without --init shows the power-off
                restore.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = ("arch::x86_64::powerctl::restore",
                "arch::x86_64::restore_power",
                "arch::x86_64::shutdown",
                "arch::x86_64::reset");
        }
        requirement <'L.x86_64.208'> powerTaskRestoreIsMasked : ItemLowLevel {
            attribute :>> statement = "A task restore -- the holder gone, the lease, the backstop, a
                write that did not read back -- shall run with interrupts masked
                on its processor, so that no interleaving of its steps with a
                power-off, reset or panic restore on another processor leaves a
                raised value with nothing in force.";
            attribute :>> criterion = "Stage 3 runs every interleaving of a task restore's 6 steps with
                an exit restore's 6, from a raise in force, on the fake, whose
                stop halts a processor that is not masked: every one ends with
                firmware's field and nothing in force. Negative control: the task
                restore unmasked panics stage 3.";
            attribute :>> parent = "H.DEV.6";
            attribute :>> unit = "arch::x86_64::powerctl::State::restore_once";
        }
        requirement <'L.x86_64.207'> powerFakeIsTestOnly : ItemLowLevel {
            attribute :>> statement = "Under ferrix.powerctl.fake alone the power node shall stand on
                fake_check's register file on any x86-64 processor: the backend
                chosen once at the probe, no file under powerctl/ reaching an MSR,
                and every kernel power line saying (fake); the word shall be
                test_only in build-flags.toml.";
            attribute :>> criterion = "cargo xtask test-power's fake rows: every kernel power line says
                (fake). check-device-access.py: no MSR access under powerctl/,
                with a planted one as its control.";
            attribute :>> parent = "H.DEV.5";
            attribute :>> unit = "arch::x86_64::powerctl::say";
        }
        requirement <'L.x86_64.32'> speculationPlanFollowsCpuid : ItemLowLevel {
            attribute :>> statement = "Plan::for_processor shall choose enhanced
                IBRS where IA32_ARCH_CAPABILITIES offers it, else AutoIBRS, else
                IBRS; STIBP unless enhanced IBRS; SSBD unless the processor reports
                it is not affected; the IBPB barrier where IBPB is offered; VERW on
                exit for an Intel processor with MD_CLEAR and without MDS_NO, and
                where L.x86_64.179 asks it for RFDS; BHI_DIS_S or the
                branch-history loop where L.x86_64.180 asks one; ERAPS in place of the in-domain refill only where CPUID 0x8000_0021
                EAX bit 24 is set and CR4.PCIDE is clear; and offered shall decode
                those bits from CPUID and IA32_ARCH_CAPABILITIES.";
            attribute :>> criterion = "For each row of a table of CPUID and
                capability words, the plan's defences and MSR values are the
                expected set.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::offered",
                "arch::x86_64::speculation::leaf_or_zero",
                "arch::x86_64::speculation::vendor",
                "arch::x86_64::speculation::Plan::for_processor",
                "arch::x86_64::speculation::store_bypass_immune",
                "arch::x86_64::speculation::mds_exposed");
        }
        requirement <'L.x86_64.33'> speculationExposureReported : ItemLowLevel {
            attribute :>> statement = "report_exposure shall print a `cpu` line
                saying, for Spectre v2 into ring 0, between programs, store bypass,
                Meltdown, MDS, RFDS and BHI, whether the plan covers it, the
                processor is not affected, or it is uncovered (AoU-11), and a second
                `cpu` line saying the same for the hazards of L.x86_64.181.";
            attribute :>> criterion = "Under TCG's qemu64 the line reports Spectre
                v2 into ring 0 not covered and Meltdown not affected; under KVM with
                auto-IBRS offered it reports it covered.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::report_exposure",
                "arch::x86_64::speculation::meltdown_exposed",
                "arch::x86_64::speculation::store_bypass_immune",
                "arch::x86_64::speculation::mds_exposure",
                "arch::x86_64::speculation::rfds_exposure",
                "arch::x86_64::speculation::bhi_exposure",
                "arch::x86_64::speculation::Exposure::fmt");
        }
        requirement <'L.x86_64.179'> registerFileClearedWhereRfdsReaches : ItemLowLevel {
            attribute :>> statement = "With the speculation switch on, rfds shall
                answer not affected for a processor whose IA32_ARCH_CAPABILITIES
                says RFDS_NO (bit 27); otherwise affected where it says RFDS_CLEAR
                (bit 28), and for an Intel processor under a hypervisor, of a family
                other than 6, of a family 6 model Linux 6.18.54's cpu_vuln_blacklist
                lists for RFDS (an Alder or Raptor Lake, 0x97 or 0xb7, only where it
                is hybrid or its core is an Atom), or of a family 6 model neither of
                that Linux's two tables names; and not affected otherwise. An
                affected processor saying RFDS_CLEAR shall have VERW run on every
                return to ring 3 -- SYSRET, IRET and the paranoid entries' restore
                -- on every processor; one without it shall be reported RFDS NOT
                covered on the exposure line.";
            attribute :>> criterion = "rfds_decisions gives each described
                processor its verdict and plan -- the N150's Gracemont with
                RFDS_CLEAR (VERW on exit), without it (NOT covered, no VERW), with
                RFDS_NO (none); every listed model with old microcode; an Alder Lake
                without and with E-cores and a Raptor Lake on an Atom core; a Kaby
                Lake on bare metal (not affected) and as a guest (affected); a guest
                without IA32_ARCH_CAPABILITIES; two unknown family 6 models and
                family 19, each without and with RFDS_CLEAR; a Zen 5 -- or the boot
                panics (FX-0307). Negative controls: the plan's RFDS arm removed,
                and the unknown model answered not affected, each panic. The VERW on
                the exits is MDS's, run when CLEAR_CPU_BUFFERS is set; no gate's
                processor is RFDS-affected with RFDS_CLEAR, so its running there is
                hardware evidence (the N150 boot's defences line, VERW on exit).";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::rfds",
                "arch::x86_64::speculation::listed",
                "arch::x86_64::speculation::Plan::for_processor",
                "arch::x86_64::speculation::Plan::add_ring3_crossings",
                "arch::x86_64::speculation::offered",
                "arch::x86_64::speculation::init");
        }
        requirement <'L.x86_64.180'> branchHistoryKeptFromRingZero : ItemLowLevel {
            attribute :>> statement = "With the speculation switch on, bhi shall
                answer not affected for an AMD or Hygon processor, and for any other
                without enhanced IBRS (IA32_ARCH_CAPABILITIES bit 1) outside a
                hypervisor; for every other processor, BHI_NO notwithstanding, the
                plan shall set IA32_SPEC_CTRL bit 10 (BHI_DIS_S) where CPUID.(7,2)
                EDX bit 4 (BHI_CTRL) and IA32_SPEC_CTRL are offered, read back with
                the register's other bits on every processor; and otherwise run
                ferrix_clear_bhb, Linux 6.18.54's clear_bhb_loop with 5 calls of 5
                taken jumps, on every entry from ring 3 before any indirect branch:
                SYSCALL through an LSTAR pointed at the entry that calls it, read
                back on every processor; a trap or interrupt from ring 3 behind a
                test whose mispredicted way meets the entry's lfence; and an NMI or
                machine check from ring 3 the same, fenced on both ways.";
            attribute :>> criterion = "bhi_decisions gives each described
                processor its verdict and plan -- the N150's Gracemont (the loop),
                with BHI_CTRL (BHI_DIS_S in the SPEC_CTRL value), with BHI_NO
                without and with BHI_CTRL (the same two), an Intel guest hiding
                enhanced IBRS (the loop), a Skylake without it on bare metal (none),
                a Zen 5 bare and as a guest (none) -- or the boot panics (FX-0307).
                Every hardened boot runs ferrix_clear_bhb once and finds LSTAR naming
                the entry the plan chose. Negative control: the plan's loop arm
                removed panics. A boot on an Intel model under TCG (Snowridge, no
                IA32_ARCH_CAPABILITIES) plans the loop and passes every stage with
                it on every entry; BHI_DIS_S's write is hardware evidence, no gate's
                processor offering BHI_CTRL.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::bhi",
                "arch::x86_64::speculation::Plan::for_processor",
                "arch::x86_64::speculation::Plan::add_ring3_crossings",
                "arch::x86_64::speculation::apply",
                "arch::x86_64::speculation::clears_branch_history",
                "arch::x86_64::speculation::clear_branch_history",
                "arch::x86_64::syscall::stub_address",
                "arch::x86_64::syscall::point_entry",
                "arch::x86_64::syscall::entry_is_planned");
        }
        requirement <'L.x86_64.181'> everyLinuxHazardNamed : ItemLowLevel {
            attribute :>> statement = "With the speculation switch on,
                other_exposures shall decide ITS, MMIO stale data, SRBDS, TAA,
                Retbleed, SRSO and TSA as Linux 6.18.54's cpu_set_bug_bits and
                vulnerable_to_its do, every stepping of a listed model taken as
                affected; answer covered only for an Intel part's Retbleed where
                enhanced IBRS is planned, and for an AMD part's SRSO where it says
                SRSO_USER_KERNEL_NO and IBPB_BRTYPE and the switch barrier is
                planned; answer every other affected one NOT covered; and answer
                VMSCAPE not affected, the item running no guest.";
            attribute :>> criterion = "exposure_decisions gives each described
                processor its eight verdicts -- the N150's Gracemont none affected;
                an Ice Lake ITS, MMIO stale data and SRBDS NOT covered, and ITS not
                affected with ITS_NO; an Intel guest ITS NOT covered; a part with
                TSX TAA NOT covered; a Skylake Retbleed NOT covered without enhanced
                IBRS and covered with it; a Zen 3 SRSO and TSA NOT covered and
                Retbleed not; a Zen 2 Retbleed NOT covered; a Zen 5 with
                SRSO_USER_KERNEL_NO and IBPB_BRTYPE nothing NOT covered and SRSO
                covered; QEMU's qemu64 none affected -- or the boot panics
                (FX-0307).";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::x86_64::speculation::other_exposures",
                "arch::x86_64::speculation::intel_listed",
                "arch::x86_64::speculation::listed",
                "arch::x86_64::speculation::its_exposure",
                "arch::x86_64::speculation::mmio_exposure",
                "arch::x86_64::speculation::srbds_exposure",
                "arch::x86_64::speculation::taa_exposure",
                "arch::x86_64::speculation::retbleed_exposure",
                "arch::x86_64::speculation::srso_exposure",
                "arch::x86_64::speculation::tsa_exposure",
                "arch::x86_64::speculation::report_exposure");
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
                and its frequency, opening the HPET first where firmware describes
                one, and going on as a machine with no HPET, with a clock WARNING
                line, where it fails to open: the invariant TSC measured over 10 ms
                of the HPET where both exist; else, with no invariant TSC, the TSC
                at the rate KVM's time page states where kvm_word vouches for it;
                else, with no HPET, the TSC measured against PIT channel 2, a rate
                under 200 MHz refused as a PIT not counting; else, with no HPET and
                no PIT rate, the TSC at the rate CPUID leaf 0x15 states (crystal
                times ratio), or failing that leaf 0x16's base frequency, an
                approximation Linux makes only for models it names; else a 64-bit
                HPET at its stated period; else the TSC measured against a 32-bit
                HPET. The PIT wait shall end after 5,000,000 polls and the HPET
                measurement's after 20,000,000.";
            attribute :>> criterion = "counter_hz agrees with a reference the
                counter was not calibrated from, within a stated tolerance, on each
                path a gate boots. Made-up CPUID leaves give the crystal's rate,
                the base frequency's, and none. A reference boot that prints a
                clock WARNING line other than TCG's HPET line fails test-boot.
                Under KVM with no HPET and no PIT the boot takes the CPUID path;
                QEMU reports neither leaf, so it stops there.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::init",
                "arch::x86_64::clock::adopt",
                "arch::x86_64::clock::hpet_address",
                "arch::x86_64::clock::open_hpet",
                "arch::x86_64::clock::invariant_tsc",
                "arch::x86_64::clock::kvm_facts",
                "arch::x86_64::clock::calibrate_tsc_against_hpet",
                "arch::x86_64::clock::start_tsc",
                "arch::x86_64::clock::pit_finished",
                "arch::x86_64::clock::calibrate_tsc",
                "arch::x86_64::clock::cpuid_tsc_hz",
                "arch::x86_64::clock::rate_of_leaves",
                "arch::x86_64::clock::write64",
                "arch::x86_64::clock::main_counter",
                "arch::x86_64::clock::read64",
                "arch::x86_64::cpu::rdtsc",
                "arch::x86_64::cpu::outb",
                "arch::x86_64::cpu::inb");
        }
        requirement <'L.x86_64.39'> counterBringUpRefusesDeadTimer : ItemLowLevel {
            attribute :>> statement = "open_hpet shall fail with a named reason
                when the HPET's counter does not advance, which clock::init takes
                for a machine with no HPET (L.x86_64.38); and clock::init shall
                fail with a named reason, not answer a counter, when with no HPET
                the PIT never reaches its terminal count or counts under 200 MHz
                and CPUID states no TSC rate.";
            attribute :>> criterion = "With the HPET's counter held still the
                boot says so on a clock WARNING line and test-boot fails it; with
                no HPET and no PIT (QEMU hpet=off,pit=off, whose CPUID states no
                rate) init answers an error.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::open_hpet",
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
        requirement <'L.x86_64.166'> counterChosenByWhoVouches : ItemLowLevel {
            attribute :>> statement = "choose shall answer the counter for a
                machine described -- the TSC by its measurement with an invariant
                TSC and an HPET, the TSC at KVM's rate when the processor reports
                no invariant TSC and kvm_word vouches, the TSC by the PIT with no
                HPET, a 64-bit HPET only when none of these holds and with the
                reason, the TSC measured against a 32-bit HPET otherwise -- and
                kvm_word shall vouch only under the hypervisor bit, KVM's
                signature, CLOCKSOURCE2, CLOCKSOURCE_STABLE_BIT, a time page read
                whole with its stable flag set, and a rate pvclock_hz answers,
                refusing with the first of these that fails.";
            attribute :>> criterion = "Nine machines described are each given
                their counter, a 32-bit HPET that could not be measured against is
                refused with or without the invariant bit, and KVM's word with each
                of its 7 conditions failing alone is refused for that reason and
                given the HPET with it.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::choose",
                "arch::x86_64::clock::kvm_word");
        }
        requirement <'L.x86_64.167'> timePageRate : ItemLowLevel {
            attribute :>> statement = "pvclock_hz shall answer the rate a time
                page's multiplier and shift state, 10^9 * 2^32 / (mul * 2^shift)
                rounded down, and nothing for a multiplier of zero, a shift outside
                -31 to 31 or a rate outside 1 MHz to 2^40 Hz.";
            attribute :>> criterion = "15 pairs of multiplier and shift, the gate
                host's own among them, are each given the rate worked out by hand
                or refused: a zero multiplier, shifts of -32 and 32, and rates
                either side of both bounds.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = "arch::x86_64::clock::pvclock_hz";
        }
        requirement <'L.x86_64.168'> timePageGivenBack : ItemLowLevel {
            attribute :>> statement = "kvm_time_page shall register the kernel's
                time page with KVM on the running processor with interrupts masked,
                and shall have given it back when it returns, whatever was read.";
            attribute :>> criterion = "Under KVM with a paravirtual clock, MSR
                4b564d01h reads 0 after bring-up and 0 again after kvm_time_page,
                whether or not it answered a page, and the rate an answered page
                states is within a third of counter_hz where the page says stable
                and the counter is the TSC; elsewhere the boot says the check did
                not run.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::kvm_time_page",
                "arch::x86_64::clock::kvm_time_page_registered");
        }
        requirement <'L.x86_64.169'> timePageReadWhole : ItemLowLevel {
            attribute :>> statement = "read_time_page shall read a time page's
                version first and last, look again when it is zero, odd or changed
                across the read, at most 1,000 times and then answer nothing, and
                shall answer tsc_to_system_mul from the page's word 6, tsc_shift
                from byte 28 and the flags from byte 29.";
            attribute :>> criterion = "A literal 32-byte page is decoded; a
                version odd then even is read on the third look; a version and
                multiplier that change across the read are answered from the second
                read; a page never filled and one that never settles answer nothing
                after 1,000 tries.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = "arch::x86_64::clock::read_time_page";
        }
        requirement <'L.x86_64.170'> hpetSaidWithItsReason : ItemLowLevel {
            attribute :>> statement = "hpet_reason shall answer a reason exactly
                when the counter counter_now reads is the HPET, from which
                warn_of_hpet prints the boot's warning that the HPET is the clock.";
            attribute :>> criterion = "On every boot hpet_reason answers a reason
                if and only if the HPET's window is the counter's.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::x86_64::clock::hpet_reason",
                "arch::x86_64::clock::warn_of_hpet",
                "arch::x86_64::clock::HpetReason::from_code");
        }
        requirement <'L.x86_64.171'> hpetOnlyWithoutATscToTrust : ItemLowLevel {
            attribute :>> statement = "clock::init shall not make the HPET the
                counter on a machine where KVM vouches for the TSC in full, nor
                where the processor reports an invariant TSC unless its measurement
                against the HPET did not come off.";
            attribute :>> criterion = "On the running machine, with its own CPUID
                and time page read again: no HPET reason where KVM vouches without
                an invariant TSC, and none but the failed measurement's with one.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = "arch::x86_64::clock::init";
        }
    }

    package SignalFrames {
        doc /* src/kernel/src/arch/x86_64/signal.rs: a 64-bit program's signal frame, its
             * return, and the context resumed from it. */

        requirement <'L.x86_64.42'> nativeSignalRoundTrip : ItemLowLevel {
            attribute :>> statement = "setup_signal_frame shall, for a handler a
                64-bit call installed that names a restorer, whatever mode the
                interrupted code ran in, write Linux's rt_sigframe below the stack
                -- pretcode the restorer, a ucontext holding the interrupted
                registers, code selector and mask, the siginfo -- and enter the
                handler in 64-bit mode with RDI the signal, RSI the siginfo and RDX
                the ucontext; restore_signal_frame shall load the registers and mask
                that frame holds, a register the handler changed through the
                ucontext included.";
            attribute :>> criterion = "The 64-bit signal program exits 77: its
                SA_SIGINFO handler for SIGUSR1 saw EDI 10, si_signo 10 at RSI and
                SIGUSR1 blocked, the 70 it added to the ucontext's saved RBX came
                back through rt_sigreturn, and the mask after the return is empty
                (the `signals` line). The WoW64 program's 64-bit SIGILL handler for
                a ud2 in its 32-bit code runs with CS 0x33 and reads cs 0x23 in its
                sigcontext (the `wow64` line); with the frame chosen by the
                interrupted code's mode it is entered on an i386 frame and the check
                fails by name.";
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
        requirement <'L.x86_64.174'> faultInTheSignalContext : ItemLowLevel {
            doc /* Wine's SIGSEGV handler tells a page fault from every other
                trap by trapno and a write from a read by err (docs/WINE.md
                §5). A signal a fault did not raise, delivered on the way back
                from an exception the kernel resolved, keeps the three zero. */
            attribute :>> statement = "put_cause shall write the siginfo, and, for a
                signal a fault raised -- the interrupted context entered by an
                exception, the signal SIGSEGV, SIGBUS, SIGILL, SIGFPE or SIGTRAP,
                and the siginfo's code above zero -- the exception's
                vector in the 64-bit sigcontext's trapno, its error code in err,
                and a page fault's address, the siginfo's, in cr2; for any other
                signal the three shall stay zero.";
            attribute :>> criterion = "A SIGSEGV handler for a write to an unmapped
                page reads trapno 14, err 6 and the address in cr2 and si_addr and
                exits 0 (the `sigsegv` line); with the three left zero the
                program exits 1 and the check fails by name.";
            attribute :>> parent = "H.TRAP.9";
            attribute :>> unit = "arch::x86_64::signal::put_cause";
        }
        requirement <'L.x86_64.175'> machineCheckTakenOnEveryProcessor : ItemLowLevel {
            doc /* F-73. With CR4.MCE clear a processor does not take the
                machine check: it shuts down, a reset with no report on most
                machines, and an Intel broadcast takes every processor there
                at once. The loader keeps the firmware's CR4, so the kernel
                sets the bit itself. The criterion reads the processor's
                state rather than trusting the firmware: a secondary starts
                from the boot processor's CR4 with the bit taken out, so one
                whose bring-up did not set it reads clear whatever firmware
                did. */
            attribute :>> statement = "enable_machine_check shall set CR4.MCE on
                every processor whose CPUID offers the machine-check exception,
                at its bring-up, after that processor's IDT and its TSS with the
                machine check's interrupt stack are loaded and before any program
                runs there, in every build.";
            attribute :>> criterion = "Every processor's bring-up sets the bit and
                is counted, and in stage 4 every processor reads its own CR4.MCE
                set, the count and the readings equalling the processors (the `mce`
                line), and a processor whose CPUID offers no machine-check exception
                fails it by name; with the bring-up's set removed, stage 4 fails by
                name. A
                machine check injected through QEMU's monitor into each processor in
                turn after stage 4 (`mce <cpu>`, test-mce) ends the boot in the FX-9005
                report, with an mce: bank line naming the record, under KVM and
                TCG.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::cpu::enable_machine_check",
                "arch::x86_64::cpu::has_machine_check",
                "arch::x86_64::take_machine_check");
        }
        requirement <'L.x86_64.43'> nativeForgedFrameRefused : ItemLowLevel {
            attribute :>> statement = "restore_signal_frame shall refuse a 64-bit
                frame whose RIP or RSP lies outside the user half, or that gives
                back ring 3's 32-bit code with a RIP at or above 4 GiB, and resume a
                frame's program with RFLAGS limited to the restorable flags and IF,
                SS ring 3's data, and CS ring 3's 32-bit code where the frame holds
                it and ring 3's 64-bit code for anything else.";
            attribute :>> criterion = "64-bit frames with a kernel-half RIP and with
                a kernel-half RSP, and one with CS 0x23 and a RIP of 4 GiB, end with
                SIGSEGV; frames asking IOPL 3, IF clear,
                TF, and CS and SS 0x10 and 0x18 resume in ring 3 with IOPL 0 and IF
                set, read by pushfq and by moving CS and SS. The WoW64 program's
                handler returns through a frame saying cs 0x23 and its 32-bit code
                reads CS 0x23 after it (the `wow64` line); with CS forced to 64-bit
                code the program exits 25 and the check fails by name; with the 4 GiB
                refusal removed the boot stops on the kernel's IRETQ.";
            attribute :>> parent = "H.TRAP.5";
            attribute :>> unit = ("arch::x86_64::signal::restore_signal_frame",
                "arch::x86_64::signal::restored_code_selector");
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
            attribute :>> statement = "setup_signal_frame shall, for a handler an
                i386 call installed, write rt_sigframe_ia32 for an SA_SIGINFO
                handler and sigframe_ia32 for a plain one and enter the handler on
                it, in compatibility mode where the program is a 32-bit one (a
                64-bit program's, on its way back from SYSCALL, is not claimed);
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
                SYSRET leaves SS the user data selector at RPL 3, and
                restore_user_state shall load SS with kernel data, a flat 4 GiB
                segment with a 32-bit stack pointer, at every switch to a task with
                user state where SS holds anything else, so that a task leaving a
                system call through SYSRET, which on AMD keeps SS's hidden part, has
                a stack its 32-bit code can use.";
            attribute :>> criterion = "A program reads SS after a system call and
                finds user data at RPL 3. A 64-bit program pinned beside a spinning
                one sleeps 50 times, reads SS 0x2b after each sleep, pushes a word
                in 32-bit code after the sleep returns through SYSRET, and reads
                every word back, and some switch found SS null and loaded it (the
                `sysret` line); on AMD, with the load removed or with kernel data's
                descriptor limit 0, the push is a stack fault and the check fails by
                name.";
            attribute :>> parent = ("H.TRAP.6", "H.TRAP.7");
            attribute :>> unit = ("arch::x86_64::syscall::init",
                "arch::x86_64::cpu::settle_kernel_ss");
        }
        requirement <'L.x86_64.59'> entryStackPerTask : ItemLowLevel {
            attribute :>> statement = "set_entry_stack and restore_user_state shall
                point both ways in from ring 3 --
                the SYSCALL entry stack and the TSS's RSP0, which STR and SGDT
                find at each load of this processor's tables and its per-CPU
                record keeps (note_tables) -- at the incoming task's kernel
                stack, so that two programs taking turns on one processor keep
                their own registers.";
            attribute :>> criterion = "Two spinning programs pinned to one
                processor, each preempted in user mode at least twice, exit 41 and
                43 with their stack pointers unchanged (the `procs` line).";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::syscall::set_entry_stack",
                "arch::x86_64::gdt::set_privilege_stack",
                "arch::x86_64::gdt::note_tables", "arch::x86_64::gdt::ask_privilege_stack",
                "arch::x86_64::switch::restore_user_state",
                "arch::x86_64::cpu::read_task_register",
                "arch::x86_64::cpu::read_gdt");
        }
        requirement <'L.x86_64.60'> archPrctlRefusals : ItemLowLevel {
            doc /* ARCH_SET_GS's refusal is half of what the paranoid entries' rule
                rests on (L.x86_64.25, arch/x86_64/paranoid.rs): a program's GS
                base never has its sign bit set, so GS_BASE itself says whose it
                is. The other half is L.x86_64.172. */
            attribute :>> statement = "arch_prctl shall refuse ARCH_SET_FS and
                ARCH_SET_GS with an address outside the user half with EPERM,
                before it writes anything, and any request but ARCH_SET_FS,
                ARCH_SET_GS, ARCH_GET_FS and ARCH_GET_GS with EINVAL.";
            attribute :>> criterion = "A program's ARCH_SET_FS and ARCH_SET_GS with
                0xffff800000000000 and its ARCH_SET_GS with 0x800000000000 answer
                -1 and its request 0x1000 -22, and it exits 0 (the first `prctl`
                line). With ARCH_SET_GS's refusal dropped the program exits 2 and
                the check fails by name.";
            attribute :>> parent = "H.TRAP.1";
            attribute :>> unit = "arch::x86_64::syscall::arch_prctl";
        }
        requirement <'L.x86_64.172'> noProgramWritesABaseWithoutTheCall : ItemLowLevel {
            doc /* The other half of the paranoid entries' rule (L.x86_64.60's
                note; docs/WINE.md §3). Stated as what a check can read: on the
                gates' processor model, which has no FSGSBASE, and on a machine
                whose firmware leaves the bit clear, the boot passes without
                the clear, so only the processor's state and the clear's effect
                on a set bit say anything. */
            attribute :>> statement = "CR4.FSGSBASE shall read clear on every
                processor from its bring-up on, before any program runs there,
                forbid_user_base_writes shall clear a set CR4.FSGSBASE, and a
                program's WRGSBASE shall be an invalid opcode.";
            attribute :>> criterion = "Every processor's bring-up reads the bit
                clear and is counted, the count equalling the processors; where
                CPUID offers FSGSBASE, the bit set in ring 0 with interrupts
                masked reads clear after forbid_user_base_writes, and where it
                does not the line says the clear was not seen; a program's
                WRGSBASE with a kernel address ends it with SIGILL on the
                checking processor and on the last application processor (the
                `gsbase` line). On a processor with FSGSBASE, with the clear
                removed, the check fails by name.";
            attribute :>> parent = "H.TRAP.6";
            attribute :>> unit = ("arch::x86_64::cpu::forbid_user_base_writes",
                "arch::x86_64::cpu::has_fsgsbase",
                "arch::x86_64::cpu::permit_user_base_writes_for_check");
        }
        requirement <'L.x86_64.173'> archPrctlReadsTheBases : ItemLowLevel {
            attribute :>> statement = "arch_prctl(ARCH_GET_FS) and
                arch_prctl(ARCH_GET_GS) shall write the calling thread's FS base
                and GS base to the address given, and answer EFAULT for an
                address that cannot be written.";
            attribute :>> criterion = "A program's ARCH_GET_FS and ARCH_GET_GS into
                address 0 answer -14; the GS base it then sets reads back through
                %gs:0 and through ARCH_GET_GS, the FS base it sets through
                ARCH_GET_FS, and the GS base is still the first; it exits 0 (the
                second `prctl` line).";
            attribute :>> parent = "H.TRAP.1";
            attribute :>> unit = "arch::x86_64::syscall::user_base";
        }
        requirement <'L.x86_64.61'> threadPointerPerTask : ItemLowLevel {
            attribute :>> statement = "arch_prctl(ARCH_SET_FS) with a user address
                shall load a null FS selector where FS holds another, and set this
                processor's FS base and the running task's recorded one, with
                interrupts masked, and restore_user_state shall load each task's
                recorded FS base where its FS selector is null; a base a null
                selector cleared comes back as recorded.";
            attribute :>> criterion = "Two 64-bit programs pinned to one processor
                each set a distinct FS base and read their own words through %fs
                10,000 times across sched_yield, and one that clears its base with
                USER_DS and a null selector reads its own word again after a
                nanosleep (the `fsbase` line); with the save reading FS_BASE again,
                that read faults and the check fails by name. The WoW64 program
                loads a thread-local selector into FS, calls ARCH_SET_FS, and reads
                FS 0 and its new base through %fs:0 (the `wow64` line); without the
                null load it exits 12.";
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
        requirement <'L.x86_64.150'> theFastPathSwitchIsReadOnce : ItemLowLevel {
            attribute :>> statement = "fastpath::init shall read ferrix.fastpath once,
                before the first program, from the loader's command line and then the
                device tree's bootargs; on shall be true only for ferrix.fastpath=on,
                off for off, for no option and for any other value, which is reported
                as ignored; it shall raise the core's fast-path flag
                (trap::set_fast_write_read) for on alone, before the first program,
                and nothing shall lower it; and stage 9 shall print which path
                channel_write_read takes and how the option was read.";
            attribute :>> criterion = "Booted with ferrix.fastpath=off and with
                ferrix.fastpath=on, x86-64's stage-9 line says off and on as asked,
                and ipc-equiv's transcripts on the two boots are the same line for
                line (`cargo xtask test-ipc-equiv`); test-boot, which passes no
                option, says off by default.";
            attribute :>> parent = "H.BOOT.2";
            attribute :>> unit = ("fastpath::init", "fastpath::on", "fastpath::report",
                "trap::set_fast_write_read");
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
        requirement <'L.x86_64.178'> x87ErrorsAreExceptionsOnEveryProcessor : ItemLowLevel {
            doc /* With CR0.NE clear an unmasked x87 error is not #MF: the
                processor signals it on the legacy FERR# line instead, and
                L.x86_64.77's SIGFPE for it rests on a bit nothing set. The
                Intel N150's AMI firmware hands over with it clear; QEMU's
                firmware leaves it set, so no emulated boot saw it. The loader
                keeps the firmware's CR0 and adds WP alone, so the kernel sets
                the x87 controls itself. The criterion reads the processor's
                state rather than trusting the firmware: a secondary starts
                from the boot processor's CR0 with NE taken out, so one whose
                bring-up did not set it reads clear whatever firmware did. */
            attribute :>> statement = "take_x87_controls shall set CR0.NE and
                CR0.MP and clear CR0.EM and CR0.TS on every processor, at its
                bring-up, before that processor saves or restores a program's x87
                or vector state and before any program runs there.";
            attribute :>> criterion = "Every processor's bring-up sets the controls
                and is counted, and in stage 4 every processor reads its own CR0
                with NE and MP set and EM and TS clear, the count and the readings
                equalling the processors (the `x87` line), before the first program
                runs on any processor (stage 4's line at 1.44 s on the gates' KVM
                boot, stage 3's programs at about 12 s); stage 3 reads CR0.NE set
                on the boot processor before its x87 program. With CR0.NE cleared
                before stage 3's look, stage 3 fails by name; with a secondary's
                set removed, stage 4 fails by name.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::x86_64::cpu::take_x87_controls",
                "arch::x86_64::cpu::x87_controls_held",
                "arch::x86_64::cpu::write_cr0");
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
        requirement <'L.x86_64.209'> aProgramsInt3IsSigtrap : ItemLowLevel {
            doc /* Linux's do_int3_user forces SIGTRAP with force_sig, so
                SI_KERNEL and no address, and records trap 3; int3 pushes the
                address after itself. Wine reads the trap number to tell a
                breakpoint from an access violation. */
            attribute :>> statement = "A 64-bit program's int3 shall raise #BP,
                for which classify and fault_signal name SIGTRAP with si_code
                SI_KERNEL, and put_cause shall write trap 3 and the address after
                the int3 into its sigcontext, where a handler's return resumes.";
            attribute :>> criterion = "A 64-bit program's SIGTRAP handler for its
                int3 reads si_signo 5, si_code 0x80, trapno 3 and rip past the
                0xcc, and is resumed after it (the `int3` line). With the #BP and
                #OF gates at DPL 0 the program is ended by SIGSEGV and the check
                fails by name.";
            attribute :>> parent = ("H.TRAP.4", "H.TRAP.9");
            attribute :>> unit = ("arch::x86_64::trap::classify",
                "arch::x86_64::trap::fault_signal",
                "arch::x86_64::signal::put_cause");
        }
        requirement <'L.x86_64.210'> anI386IntoIsOverflow : ItemLowLevel {
            doc /* Linux's do_error_trap for X86_TRAP_OF forces SIGSEGV with
                force_sig, so SI_KERNEL and no address, and records trap 4.
                into exists only outside 64-bit code. */
            attribute :>> statement = "An i386 program's into with OF set shall
                raise #OF, for which fault_signal names SIGSEGV with si_code
                SI_KERNEL, and its int3 #BP, SIGTRAP with SI_KERNEL; sigcontext_of
                shall write trap 4 and trap 3 and the address after each
                instruction into the i386 sigcontext, where a handler's return
                resumes.";
            attribute :>> criterion = "An i386 program's handler reads SIGSEGV,
                0x80, trap 4 and eip past its into, then SIGTRAP, 0x80, trap 3 and
                eip past its int3, and is resumed after each (the `into` line).
                With the #OF gate alone at DPL 0 the handler reads trap 13 and the
                check fails by name.";
            attribute :>> parent = ("H.TRAP.4", "H.TRAP.9");
            attribute :>> unit = ("arch::x86_64::trap::classify",
                "arch::x86_64::trap::fault_signal",
                "arch::x86_64::signal::compat::sigcontext_of");
        }
        requirement <'L.x86_64.211'> tscAuxNamesItsOwnProcessor : ItemLowLevel {
            doc /* Linux's setup_getcpu writes (node << 12) | cpu there and
                Windows the processor number; a program compares RDTSCP's ECX
                or RDPID with getcpu or GetCurrentProcessorNumber. */
            attribute :>> statement = "Every processor that has RDTSCP or RDPID
                shall hold its logical processor number, node 0, in IA32_TSC_AUX
                before it first returns to user mode.";
            attribute :>> criterion = "On every processor with RDTSCP, its ECX read
                back at bring-up equals the processor's logical number (the
                `tsc_aux` cpu line counts N of N); with the write removed the boot
                ends in FX-0310 by name. RDPID reads the same register.";
            attribute :>> parent = "H.SCHED.6";
            attribute :>> unit = ("arch::x86_64::syscall::init",
                "arch::x86_64::syscall::tsc_aux_of",
                "arch::x86_64::syscall::has_tsc_aux",
                "arch::x86_64::syscall::record_tsc_aux");
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
            doc /* The processor checks the gate's DPL for int3, into and
                int n alike. Until 2026-10-23 only the legacy system call's
                gate had DPL 3, though this statement already named the
                breakpoint, and no check read the table: a program's int3 was
                #GP, SIGSEGV with no address where Linux sends SIGTRAP, and
                Wine, reading trap 13, reported an access violation for a
                breakpoint Windows code catches (two Endfield crash dumps on
                the N150, both on a deliberate int3 in UnityPlayer.dll).
                Linux gives #BP, #OF and int $0x80 system gates
                (arch/x86/kernel/idt.c, SYSG) and no others. */
            attribute :>> statement = "trap::init shall install the gates of #BP
                (vector 3), #OF (vector 4) and int $0x80 with DPL 3 and every
                other gate with DPL 0, so that ring 3 can raise those three
                vectors by a software interrupt and no other.";
            attribute :>> criterion = "After the fault programs have run, the boot
                reads all 256 gates and finds DPL 3 on vectors 3, 4 and 0x80 alone
                (the `gates` line); with a fourth vector given DPL 3 the check
                fails by name.";
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
            attribute :>> statement = "shutdown shall announce the power-off,
                drain the console and power the machine off, and shall say so
                where the power-off write returned.";
            attribute :>> criterion = "After init exits, FERRIX-POWER-OFF is printed
                with no FERRIX-POWER-OFF-FAILED after it, QEMU exits by itself with
                status 33, and init's exit line is in the log (F-46).";
            attribute :>> parent = "H.BOOT.6";
            attribute :>> unit = ("arch::x86_64::shutdown",
                "arch::x86_64::power_off",
                "arch::x86_64::cpu::debug_exit",
                "arch::x86_64::cpu::outl",
                "console::announce_power_off",
                "console::announce_power_off_failed");
        }
        requirement <'L.x86_64.212'> theSoftOffIsRecordedWhole : ItemLowLevel {
            doc /* ACPI 6.5 7.4.2 and 16.1.7; the consultant's OK IF, ledger
                line 4704, conditions 3 and 4. The AML is matched, not run:
                ferrix_acpi::sleep::s5_sleep_types. */
            attribute :>> statement = "At interrupt bring-up the kernel shall
                record what entering S5 needs -- the FADT's PM1a and PM1b
                control ports, SLP_TYPa and SLP_TYPb
                from the DSDT's Name (_S5, Package) -- all of it or nothing:
                nothing on a hardware-reduced machine, for a PM1 control block
                absent, outside I/O space or past 16 bits, a PM1_CNT_LEN under
                2, a DSDT that fails its signature, length or checksum, and no
                single constant _S5 package. SMI_CMD and ACPI_ENABLE are
                recorded with them, an SMI_CMD past 16 bits as none (no
                handoff).";
            attribute :>> criterion = "ferrix-acpi's host tests: q35's and a
                firmware's _S5 bytes read, every constant encoding and PkgLength
                size, every truncation and arbitrary bytes no answer and no
                panic, a method, a string and disagreeing packages no answer,
                and each fail-closed case of the FADT and the DSDT recording
                nothing.";
            attribute :>> parent = "H.BOOT.6";
            attribute :>> unit = ("arch::x86_64::record_sleep_registers");
        }
        requirement <'L.x86_64.213'> theMachineEntersSoftOff : ItemLowLevel {
            doc /* ACPI 6.5 4.8.3.2 and 16.1.7, Linux's acpi_hw_legacy_sleep;
                the consultant's OK IF, ledger line 4704, conditions 1, 2 and
                10. No _PTS is run (no AML interpreter), and the other
                processors are not stopped first, as for shutdown and reset:
                a machine S5 does not take is said and halted. */
            attribute :>> statement = "power_off shall, after the debug-exit write
                returned and with interrupts masked, where S5 was recorded,
                hand the machine to ACPI through SMI_CMD when SCI_EN is clear
                (waiting a bounded time), write SLP_TYP to PM1a and PM1b, then
                SLP_TYP with SLP_EN, and only then say the power-off failed and
                halt.";
            attribute :>> criterion = "cargo xtask test-iso: the live session's
                person types poweroff on a QEMU line with no isa-debug-exit,
                FERRIX-POWER-OFF is printed with no FERRIX-POWER-OFF-FAILED
                after it, and QEMU exits by itself with status 0; test-boot
                x86_64 still exits 33 (L.x86_64.98).";
            attribute :>> parent = "H.BOOT.6";
            attribute :>> unit = ("arch::x86_64::power_off",
                "arch::x86_64::acpi_power_off",
                "arch::x86_64::cpu::outw",
                "arch::x86_64::cpu::inw");
        }
        requirement <'L.x86_64.214'> pcidsOnByTheBootProcessorsCpuid : ItemLowLevel {
            doc /* docs/PCID.md; the consultant's OK IF P1-P12, ledger line
                4734, P1. By CPUID, never by vendor or part. */
            attribute :>> statement = "pcid::init shall, on the boot processor and
                before the speculation plan reads CR4 and the secondaries copy it,
                set CR4.PCIDE and turn PCIDs on for the machine exactly where
                CPUID.1:ECX[17] is set and CR3[11:0] is zero, and leave both off
                otherwise.";
            attribute :>> criterion = "The boot's slot check finds PCIDs on exactly
                where the boot processor's CPUID offers them: off on every gated
                row (QEMU's TCG offers no PCID, nazuna's KVM has none), on on the
                N150 (the `pcid` boot line).";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("arch::x86_64::pcid::init",
                "arch::x86_64::pcid::on",
                "arch::x86_64::pcid::offered_here",
                "arch::x86_64::pcid::invpcid_here",
                "arch::x86_64::pcid::report",
                "arch::x86_64::cpu::read_cr3",
                "arch::x86_64::init_speculation");
        }
        requirement <'L.x86_64.215'> aSecondaryWithoutPcidsStopsTheBoot : ItemLowLevel {
            doc /* P1: PCIDs only where every processor has them. */
            attribute :>> statement = "A secondary processor shall, before it adopts
                the boot processor's CR4, stop the boot with FX-0410 where that CR4
                has PCIDE set and its own CPUID.1:ECX[17] is clear.";
            attribute :>> criterion = "Argued: no emulated machine offers PCIDs,
                and none offers different features to its processors; every boot
                that comes up has passed the test on each secondary.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("arch::x86_64::pcid::require_on_secondary");
        }
        requirement <'L.x86_64.216'> aSlotHitKeepsOnlyWithoutAShootdownSince : ItemLowLevel {
            attribute :>> statement = "With PCIDs on, installing a space whose id a
                slot of the processor holds shall write CR3 with that slot's PCID
                and bit 63 set only where the slot's recorded shootdown count is at
                least the space's, read after the processor joined its set, and
                otherwise with bit 63 clear, recording that count.";
            attribute :>> criterion = "The slot rule on described spaces: a hit with
                no shootdown since keeps, a hit after one flushes and keeps after,
                every written value's bit 63 matching; and on PCID hardware the
                switch back after a shootdown flushes and reads the moved page's
                new frame. With the comparison taken out the boot fails.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("arch::x86_64::pcid::install",
                "arch::x86_64::pcid::Slots::choose",
                "arch::x86_64::pcid::Slots::flushed",
                "arch::x86_64::pcid::tagged_root",
                "arch::x86_64::install_user_root");
        }
        requirement <'L.x86_64.217'> aSlotMissTakesTheNextSlotAndFlushes : ItemLowLevel {
            attribute :>> statement = "With PCIDs on, installing a space whose id no
                slot holds shall take the processor's next slot round robin, record
                the space's id in it, and write CR3 with that slot's PCID, 1 to 6,
                and bit 63 clear; a slot shall match only the space whose id it
                records, and no id shall be given twice.";
            attribute :>> criterion = "The slot rule on described spaces: a new space
                flushes, a seventh takes a slot and its owner flushes when it comes
                back, every PCID in 1 to 6; a space made after another is dropped
                keeps no slot. With the miss keeping, the boot fails.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("arch::x86_64::pcid::Slots::choose",
                "user::space::next_id",
                "user::space::AddressSpace::assemble");
        }
        requirement <'L.x86_64.218'> uninstallFlushesPcidZero : ItemLowLevel {
            attribute :>> statement = "uninstall_user_root shall load the kernel's
                root with PCID 0 and bit 63 clear; PCID 0 shall tag no user
                space.";
            attribute :>> criterion = "After every uninstall of the switch-back
                check, CR3's PCID reads 0.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("arch::x86_64::uninstall_user_root",
                "arch::x86_64::pcid::tag_now");
        }
        requirement <'L.x86_64.219'> withoutPcidsTheBareRoot : ItemLowLevel {
            attribute :>> statement = "With PCIDs off, install_user_root shall write
                the space's root to CR3 and nothing else.";
            attribute :>> criterion = "On every gated x86-64 row the switch-back
                check finds each install's written value equal to the space's root,
                and the processor joining and leaving the space's set where it did
                before PCIDs; the IPC round trip on nazuna within 1% of develop's.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("arch::x86_64::pcid::install",
                "arch::x86_64::pcid::last_write",
                "arch::x86_64::last_root_write");
        }
        requirement <'L.x86_64.220'> erapsNeverBesidePcids : ItemLowLevel {
            doc /* Keeps ledger line 392's ERAPS guarantee unchanged. */
            attribute :>> statement = "ERAPS shall stand in for the in-domain return
                stack refill only on a machine where PCIDs are off, so that every
                CR3 write a switch makes flushes.";
            attribute :>> criterion = "The boot's speculation check finds the plan's
                ERAPS and the machine's PCIDs never both on, and the ERAPS decision,
                asked for a processor described as offering ERAPS, refusing it
                where CR4.PCIDE is set.";
            attribute :>> parent = "H.TRAP.17";
            attribute :>> unit = ("arch::x86_64::speculation::eraps_empties_on_switch",
                "arch::x86_64::speculation::switch_barrier_in_domain");
        }
        requirement <'L.x86_64.221'> machineCheckBanksBroughtUp : ItemLowLevel {
            doc /* docs/MACHINE-CHECK.md §3.1. The banks are enabled the way
                Linux's __mcheck_cpu_init_generic enables them; an enabled
                error is one the processor reports, never one it causes. */
            attribute :>> statement = "On every x86-64 processor whose CPUID
                offers the machine-check exception and architecture, the kernel
                shall, once that processor has its record and before any program
                runs there, read IA32_MCG_CAP and IA32_MCG_STATUS through the
                checked MSR read, write all ones to IA32_MCG_CTL where MCG_CTL_P
                is set and to each bank's MCi_CTL (bank 0 excepted on Intel family
                6 before model 0x1a), and record the bank count and MCG_SER_P for
                that processor.";
            attribute :>> criterion = "Every gate boot's /proc/ferrix-mce reads
                a non-zero bank count for every processor (test-mce-corrected
                under KVM and TCG), and the mce: line names the count; a
                processor without a record stops on a machine check
                (L.x86_64.224's table).";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::bring_up",
                "arch::x86_64::mce::enable",
                "arch::x86_64::mce::refuse",
                "arch::x86_64::mce::refused",
                "arch::x86_64::mce::start",
                "arch::x86_64::mce::start_poll",
                "arch::x86_64::mce::family_model",
                "arch::x86_64::mce::has_banks",
                "arch::x86_64::mce::intel");
        }
        requirement <'L.x86_64.222'> recordsLeftBeforeTheBootLogged : ItemLowLevel {
            doc /* §3.1 step 2: the one record of why the boot before died, on
                parts whose warm reset keeps MCi_STATUS. */
            attribute :>> statement = "Bring-up shall read every bank's
                MCi_STATUS before it enables any bank, hand each valid record with
                its address and misc where valid to the processor's poll as one
                left from before this boot, count it, and clear the bank; the poll
                shall print it as mce: cpu <n> bank <b> left from before this
                boot.";
            attribute :>> criterion = "test-mce-left: a corrected error injected
                into bank 1 of processor 0, then the monitor's system_reset; the
                next boot prints mce: cpu 0 bank 1 left from before this boot,
                under TCG (and under KVM where KVM keeps the banks across a reset,
                measured).";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::bring_up",
                "arch::x86_64::mce::keep_left",
                "arch::x86_64::mce::poll");
        }
        requirement <'L.x86_64.223'> pollLogsAndClearsCorrected : ItemLowLevel {
            attribute :>> statement = "One kernel task pinned to each processor
                with a bring-up record shall, every ferrix.mce.poll seconds (1 to
                3600 by msrlog's chooser; 5 otherwise, said), print each bank's
                corrected or uncorrected-no-action record as one mce: line with
                the processor, bank, class, status, address and misc where valid
                and the processor's running count, count it, and clear the bank;
                and shall leave every other uncorrected record in its bank.";
            attribute :>> criterion = "test-mce-corrected under KVM and TCG: a
                corrected error injected into bank 1 of every processor is printed
                exactly once per processor as mce: cpu <n> bank 1 corrected and
                retires no frame. Negative control: the poll's clear removed, the
                line repeats and the row fails.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::poll",
                "arch::x86_64::mce::run",
                "arch::x86_64::mce::say",
                "arch::x86_64::mce::Line::write_str");
        }
        requirement <'L.x86_64.224'> machineCheckClassifierFailsClosed : ItemLowLevel {
            attribute :>> statement = "The classifier shall put a record with VAL
                clear in no class, one with UC clear in corrected, one with PCC
                or OVER set or without MCG_SER_P in fatal, one with S and AR
                clear or S set and EN clear in uncorrected-no-action, S with EN
                in action-optional, S, AR and EN in action-required, and AR
                without S in fatal; and shall take an address as one page only
                with ADDRV and MISCV, and with MCG_SER_P a physical address mode
                and recoverable bits at most 12.";
            attribute :>> criterion = "Stage 3 runs the classifier and the
                address rule over a table of every class and every bit that moves
                a record between them; any difference stops the boot.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::decode::classify",
                "arch::x86_64::mce::decode::usable_address",
                "arch::x86_64::mce::decode::Class::name");
        }
        requirement <'L.x86_64.225'> bystanderMachineCheckReturns : ItemLowLevel {
            doc /* The consultant's M1: an Intel machine check is broadcast to
                every processor unless LMCE is on, and some banks are shared, so
                a processor that did not consume the poison sees the record with
                EIPV clear and RIPV set. */
            attribute :>> statement = "A machine check whose banks hold nothing
                but corrected, uncorrected-no-action and action-required records
                with EIPV clear and RIPV set, or no valid record, shall return
                with MCG_STATUS cleared and be counted as spurious, leaving those
                banks as they were, where RIPV is set; and stop where it is
                clear.";
            attribute :>> criterion = "Stage 3's table of actions, folds and
                finishes; test-mce-broadcast under KVM and TCG on two processors:
                the processor given the record with EIPV clear counts one spurious
                check and the kernel goes on. Negative control: the rule removed,
                the bystander stops the boot.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::decode::action",
                "arch::x86_64::mce::decode::fold",
                "arch::x86_64::mce::decode::finish",
                "arch::x86_64::mce::decode::rank",
                "arch::x86_64::mce::decide");
        }
        requirement <'L.x86_64.226'> actionOptionalHandedToThePoll : ItemLowLevel {
            attribute :>> statement = "A machine check with RIPV set whose
                worst record is action-optional shall copy each such record into
                a free pending slot of its processor through the slot protocol
                (empty to writing by compare and exchange, full by a release
                store), clear its bank and MCG_STATUS, and return; the poll shall
                print it and retire its frame where the address is usable; a
                machine check that finds no free slot shall stop.";
            attribute :>> criterion = "Stage 3's slot check: slots fill to
                their number and refuse the next, a slot held by a writer is
                passed by both ways, records come out whole. test-mce-srao under
                KVM and TCG: the poll prints mce: cpu <n> bank 1 action-optional
                and mce: frame <page> retired, the victim is not signalled, the
                kernel goes on.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::put",
                "arch::x86_64::mce::take",
                "arch::x86_64::mce::defer_banks",
                "arch::x86_64::mce::on_kernel_stack",
                "arch::x86_64::mce::record_of",
                "arch::x86_64::mce::read_bank",
                "arch::x86_64::mce::clear_bank",
                "arch::x86_64::mce::end_machine_check",
                "arch::x86_64::mce::retire");
        }
        requirement <'L.x86_64.227'> fatalMachineCheckPrintsTheBanks : ItemLowLevel {
            attribute :>> statement = "A machine check that stops shall print,
                in its FX-9005 report, MCG_STATUS, the bank count and every valid
                bank's MCi_STATUS, MCi_ADDR, MCi_MISC and class on a line
                beginning mce: bank, leaving the banks uncleared.";
            attribute :>> criterion = "test-mce under KVM and TCG: the PCC record
                injected into every processor ends the boot in the FX-9005 report
                with an mce: bank 0 line naming it fatal.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::mce::report",
                "arch::x86_64::mce::stop",
                "arch::x86_64::trap::report_trap");
        }
        requirement <'L.x86_64.228'> programsPoisonIsItsSigbus : ItemLowLevel {
            attribute :>> statement = "A machine check taken in ring 3 shall
                leave its interrupt stack for the task's kernel stack, read its
                banks before any other work of the trap path with interrupts
                masked, and, where one action-required record has EIPV set and a
                usable address, clear that bank and MCG_STATUS, retire the
                record's frame, and raise SIGBUS on the running program with
                si_code BUS_MCEERR_AR, si_addr the lowest address in the
                program's half that maps the frame (0 past 2^18 leaves) and
                si_addr_lsb 12, ending the program without its handler where RIPV
                is clear.";
            attribute :>> criterion = "test-mce-srar under KVM and TCG: the victim's
                handler prints code=4, its own page's address, lsb=12 and err=0
                (no physical address in its context), the frame is listed in
                /proc/ferrix-mce-retired and a second program runs;
                test-mce-kill, the same with RIPV clear: the victim is killed by
                signal 7, its handler does not run, a second program runs. Negative controls: the classifier calling the record fatal,
                and vector 18 not moved to the task stack, each stop the boot.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::from_user",
                "arch::x86_64::mce::marked",
                "arch::x86_64::mce::signal_for",
                "arch::x86_64::trap::ferrix_trap_entry",
                "trap::user_fault",
                "trap::user_fault_ending",
                "user::space::AddressSpace::find_frame",
                "mm::for_each_leaf_in");
        }
        requirement <'L.x86_64.229'> kernelsPoisonStops : ItemLowLevel {
            attribute :>> statement = "A machine check taken in ring 0 shall
                stay on its interrupt stack and stop as FX-9005 where any record is
                fatal, or action-required with EIPV set, or action-optional with
                RIPV clear, or where two records each ask for a signal.";
            attribute :>> criterion = "Stage 3's table of actions (an
                action-required record from ring 0 with EIPV set stops) and
                test-mce under KVM and TCG.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::x86_64::mce::on_kernel_stack",
                "arch::x86_64::paranoid::ferrix_paranoid_entry");
        }
        requirement <'L.x86_64.230'> machineCheckCountsInProc : ItemLowLevel {
            attribute :>> statement = "/proc/ferrix-mce, mode 0444, shall give
                whether recovery is on and, for every processor, its bank count
                and its counts of machine checks taken, recovered, spurious,
                corrected and uncorrected-no-action records, records left from
                before the boot and poll rounds, and no physical address.";
            attribute :>> criterion = "test-mce-srar prints the file before and
                after: checks and recovered go from 0 to 1 on the consumer; the
                file's text holds no 0x.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::render",
                "arch::x86_64::render_machine_check");
        }
        requirement <'L.x86_64.231'> recoveryOffStopsEveryCheck : ItemLowLevel {
            attribute :>> statement = "With ferrix.mce.recover=off on the
                command line, read when the poll starts, every machine check shall
                stop as FX-9005, as before recovery existed, and /proc/ferrix-mce
                shall say recover off.";
            attribute :>> criterion = "test-mce-srar with ferrix.mce.recover=off:
                the injection stops the boot as FX-9005.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::start",
                "arch::x86_64::mce::read_options");
        }
        requirement <'L.x86_64.232'> machineCheckBankCapAndCheckedWrites : ItemLowLevel {
            attribute :>> statement = "Bring-up shall read at most 32 banks,
                saying so where MCG_CAP names more, and shall make every bring-up
                read and write of a machine-check register through the checked
                accesses, so that a register the processor refuses leaves that
                processor without a record and is said, rather than stopping
                it.";
            attribute :>> criterion = "xtask's host test reads bring_up's body
                from the kernel's source and fails on any unchecked read_msr or
                write_msr in it, and on a bank cap other than 32; no emulator
                refuses these registers, so the refusal's line is argued.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("arch::x86_64::mce::bring_up");
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
                cached translation on its processor, global ones included and
                under every PCID, by toggling CR4.PGE and reloading CR3.";
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
        requirement <'L.trap.8'> aTrapAnswersOnlyTheCallsOwnRegister : ItemLowLevel {
            attribute :>> statement = "A Trap verdict shall carry nothing, and ask
                shall answer it with the architecture's syscall_rollback_value of
                the call as the entry read it and no other value: a value taken from
                the caller's own registers, so that nothing of the kernel's can be
                put in a program's return register, and one that is never a restart
                code the dispatcher marks.";
            attribute :>> criterion = "A slot with a filter that answers Trap is
                asked about a call with its first argument 0x1111, -512, -516 and
                -1; each answer is the architecture's rollback value of that call,
                the number on x86-64 and the first argument on the Arm pair (the
                `seccomp` line).";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = "trap::ask";
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
            attribute :>> unit = ("trap::filter_system_call", "trap::set_quiet_syscall_filter",
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
                its local APIC. The boot processor shall choose x2APIC mode where
                the platform locks legacy xAPIC (IA32_XAPIC_DISABLE_STATUS),
                firmware left x2APIC on, or ferrix.apic=x2apic asks and CPUID
                offers it, and xAPIC mode otherwise; every processor shall run in
                that mode, every local APIC register access going through one
                accessor that picks the MSR or the MMIO window. In xAPIC mode one
                left in x2APIC mode shall be taken to disabled and then to xAPIC,
                and one locked there shall stop the boot naming the processor.";
            attribute :>> criterion = "On x86-64 under KVM and TCG the check puts
                the first application processor in x2APIC mode before its own look;
                every processor looks, that one is switched to xAPIC and comes up,
                and stage 4's apic line counts them; with secondary_start's look
                removed that processor never comes up and stage 4 fails. With
                ferrix.apic=x2apic every processor says it runs in x2APIC mode and
                the boot passes; with the x2APIC end-of-interrupt write removed it
                never reaches stage 3's line.";
            attribute :>> parent = ("H.IRQ.1", "H.FAIL.1");
            attribute :>> unit = ("arch::x86_64::apic::leave_x2apic",
                "arch::x86_64::apic::choose_mode", "arch::x86_64::apic::enter_mode",
                "arch::x86_64::apic::read_reg", "arch::x86_64::apic::write_reg",
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
