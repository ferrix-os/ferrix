/*
 * Ferrix — SysML v2 model, part 19: the low-level requirements of
 * arch/aarch64.
 *
 * What each unit of src/kernel/src/arch/aarch64/ does, as `ItemLowLevel`
 * requirements (part 13 defines the format, part 14 is the pilot this
 * copies): the trap path and the signal frames it builds, the context switch,
 * the interrupt controllers and the generic timer, the boot console, the
 * system registers, translation and the TLB, starting the other processors,
 * the speculation defences, and what firmware is asked for -- power, reset,
 * entropy. The PL011 and the GICv2 are arch/arm_common's, shared with
 * ARMv7-A, and are not here.
 *
 * What proves what. This architecture's own checks are arch/aarch64/check.rs
 * (the machine check: exception syndromes decoded, console= values, the
 * TRNG against scripted firmware, the GIC's driver per description, masking
 * read back at the controller, refusals, a system call rewound),
 * arch/aarch64/trap/check.rs (real programs whose own instructions fault,
 * each required to end with the signal Linux sends) and
 * arch/aarch64/speculation/check.rs (the part tables, firmware's answers,
 * the entry loop's count). Most of the rest is exercised by every boot --
 * the console carries the log, the timer ticks, the secondaries start, the
 * scheduler switches -- but through checks in product code (main.rs's stage
 * checks) or through generic checks whose criteria are the generic layer's,
 * so it waits in the baseline as check-writing work rather than being
 * credited to a check that does not assert it. Where the hardware a unit
 * drives is not on the machine the suite boots -- the crosvm guest's 16550,
 * the Pixel 7's ramoops zone and watchdogs, a firmware TRNG -- the criterion
 * says which machine shows it.
 *
 * Ids are `L.aarch64.<n>`, flat through the architecture; the package gives
 * the area. The pilot's rules hold: a requirement names every function that
 * carries its behaviour; it is no broader than one check can prove; an
 * accessor needs no requirement of its own; and check code living in a
 * product file is listed in tools/common/data/traceability-units.json.
 */
package FerrixAarch64Requirements {
    private import FerrixItemRequirements::*;

    package Traps {
        doc /* src/kernel/src/arch/aarch64/trap.rs: the vector table, the save and
             * restore sequences, and the decoding of a syndrome into the
             * architecture-neutral trap the generic dispatcher acts on. */

        requirement <'L.aarch64.1'> syndromesAreDecoded : ItemLowLevel {
            attribute :>> statement = "classify shall turn a trap frame into the
                trap its vector entry and exception class name: a breakpoint, a
                system call, an undefined instruction, a data or instruction abort
                as a page fault with its address, access and origin, an IRQ entry as
                an interrupt, and any other class, allocated or not, as a named
                fault; and came_from_user shall answer from the vector entry.";
            attribute :>> criterion = "The machine check's 14 frames, one per class
                a boot or a program raises and one the architecture has not
                allocated, are each classified as the trap listed for them, and a
                frame from EL0 and one from EL1 are told apart.";
            attribute :>> parent = ("H.TRAP.4", "H.TRAP.6");
            attribute :>> unit = ("arch::aarch64::trap::classify", "arch::aarch64::trap::abort",
                "arch::aarch64::trap::is_irq", "arch::aarch64::trap::class_name",
                "arch::aarch64::trap::TrapFrame::came_from_user",
                "arch::aarch64::trap::TrapFrame::exception_class",
                "arch::aarch64::trap::TrapFrame::syndrome");
        }
        requirement <'L.aarch64.2'> faultsGetLinuxsSignal : ItemLowLevel {
            attribute :>> statement = "fault_signal shall give a program's
                unresolved trap the signal, si_code and address Linux's
                arch/arm64 gives it: SIGTRAP for a breakpoint, SIGILL for an
                undefined or unallocated instruction class, SIGSEGV with MAPERR or
                ACCERR for an abort, SIGBUS with ADRALN for a misaligned program
                counter or stack pointer, SIGFPE for a trapped floating-point
                exception.";
            attribute :>> criterion = "Each of the machine check's 12 faulting
                frames is given the signal, si_code and address listed for it.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = "arch::aarch64::trap::fault_signal";
        }
        requirement <'L.aarch64.3'> programsFaultToTheirSignal : ItemLowLevel {
            attribute :>> statement = "A program whose own instruction takes a
                misaligned program counter, an undefined instruction, a breakpoint
                or a read past the end of a file it mapped shall be ended by
                SIGBUS, SIGILL, SIGTRAP and SIGBUS respectively, the kernel going
                on: the vector table's entry from EL0 saves the frame, hands it to
                ferrix_trap_entry and returns through resume_user.";
            attribute :>> criterion = "Four programs, each one of those
                instructions and then exit_group(97), are run from the trap check,
                and each ends with the status 128 plus its signal.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::aarch64::trap::ferrix_trap_entry",
                "arch::aarch64::trap::resume_user", "arch::aarch64::trap::enter_user",
                "arch::aarch64::trap::init", "arch::aarch64::init_traps",
                "arch::aarch64::cpu::write_vbar");
        }
        requirement <'L.aarch64.4'> systemCallsAreServed : ItemLowLevel {
            attribute :>> statement = "system_call shall refuse an svc taken
                from EL1, answer rt_sigreturn itself by replacing the whole frame,
                and otherwise pass x8 and x0 to x5 to the dispatcher and write its
                result into x0, or begin the program an execve or clone child
                enters at its entry and stack.";
            attribute :>> criterion = "Every program the boot runs makes its calls
                through this path; a check that an svc from EL1 is refused and
                that a call's six arguments and its number reach the dispatcher
                unchanged is not written.";
            attribute :>> parent = ("H.TRAP.10", "H.TRAP.12", "H.TRAP.6");
            attribute :>> unit = ("arch::aarch64::trap::system_call",
                "arch::aarch64::trap::UserRegs::for_child",
                "arch::aarch64::trap::UserRegs::set_stack",
                "arch::aarch64::trap::UserRegs::stack_pointer",
                "arch::aarch64::decode_syscall", "arch::aarch64::decode_compat_syscall",
                "arch::aarch64::image_abi");
        }
        requirement <'L.aarch64.5'> kernelTrapsAreReported : ItemLowLevel {
            attribute :>> statement = "report_trap shall print the interrupted
                state -- the exception class by name, the syndrome, the fault
                address and the registers -- without taking the console's log, for
                a trap that stops the kernel.";
            attribute :>> criterion = "A kernel trap's failure report carries the
                class name, esr, far and the 31 registers; no boot check provokes
                one, and a deliberate crash is the failure path's evidence.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::aarch64::trap::report_trap",
                "arch::aarch64::trap::TrapFrame::instruction_pointer",
                "arch::aarch64::frame_pointer", "arch::aarch64::cpu::frame_pointer");
        }
        requirement <'L.aarch64.6'> ownBreakpointsAreSteppedOver : ItemLowLevel {
            attribute :>> statement = "A breakpoint the kernel raised itself
                shall be stepped over and returned from: advance_past_breakpoint
                moves the return address past the brk, which AArch64 leaves
                pointing at it.";
            attribute :>> criterion = "Stage 3's check raises two breakpoints; both
                reach the handler and return to the instruction after them, a
                register kept across them unchanged.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::aarch64::trap::breakpoint",
                "arch::aarch64::trap::advance_past_breakpoint");
        }
        requirement <'L.aarch64.7'> perCpuRecordIsTheProcessors : ItemLowLevel {
            attribute :>> statement = "set_cpu_local shall point TPIDR_EL1 at
                this processor's own per-CPU record, and cpu_local and
                cpu_local_register shall read it back, so that the trap path finds
                this processor's state whatever a program left in its registers.";
            attribute :>> criterion = "Each processor's per-CPU record is its own:
                a check that every online processor reads back the record it
                installed is not written.";
            attribute :>> parent = "H.SCHED.6";
            attribute :>> unit = ("arch::aarch64::set_cpu_local", "arch::aarch64::cpu_local",
                "arch::aarch64::cpu_local_register", "arch::aarch64::cpu::write_tpidr_el1",
                "arch::aarch64::cpu::read_tpidr_el1");
        }
    }

    package Signals {
        doc /* src/kernel/src/arch/aarch64/signal.rs: the rt_sigframe Linux builds
             * on AArch64 and the restore that reads it back, and the rewind of
             * a system call a signal interrupted. */

        requirement <'L.aarch64.8'> interruptedCallsAreRewound : ItemLowLevel {
            attribute :>> statement = "rewind_syscall shall make an interrupted
                svc run again when the program resumes: x0 gets its first argument
                back, the return address steps back over the svc, and x8 keeps the
                program's number, or becomes restart_syscall's when the call's
                restart is restart_syscall's.";
            attribute :>> criterion = "A clock_nanosleep frame rewound both ways
                has x0 its argument, the return address four bytes back, and x8
                clock_nanosleep's number and restart_syscall's respectively.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::aarch64::signal::UserContext::rewind_syscall",
                "arch::aarch64::signal::UserContext::from_trap",
                "arch::aarch64::signal::UserContext::store_trap");
        }
        requirement <'L.aarch64.9'> signalFramesAreLinuxs : ItemLowLevel {
            attribute :>> statement = "setup_signal_frame shall write an
                rt_sigframe as arch/arm64 lays it out -- siginfo, ucontext with the
                registers and one fpsimd_context, a frame record -- 16-byte aligned
                below the stack pointer, and enter the handler with x0 the signal,
                x1 the siginfo, x2 the ucontext and x30 the restorer.";
            attribute :>> criterion = "A handler entered from a frame this writes
                reads its signal, its siginfo and its interrupted registers where
                musl reads them; the generic signal checks run handlers but assert
                the generic layer's behaviour, and a check of the frame's layout
                on this architecture is not written.";
            attribute :>> parent = "H.TRAP.9";
            attribute :>> unit = ("arch::aarch64::signal::setup_signal_frame",
                "arch::aarch64::signal::write_frame_record",
                "arch::aarch64::signal::UserContext::stack_pointer",
                "arch::aarch64::signal::UserContext::syscall_result",
                "arch::aarch64::signal::UserContext::set_syscall_result");
        }
        requirement <'L.aarch64.10'> sigreturnKeepsPrivilege : ItemLowLevel {
            attribute :>> statement = "restore_signal_frame shall refuse a frame
                whose stack pointer is not 16-byte aligned or whose records hold no
                single fpsimd_context, and shall load the registers from it with
                PSTATE's mode, interrupt masks and execution state forced to EL0's.";
            attribute :>> criterion = "A frame forged with EL1 in PSTATE and masked
                interrupts returns to EL0 with them clear, and a misaligned or
                fpsimd-less frame is refused; not written as a check here (the
                x86-64 part of H.TRAP.5 has its forged frames).";
            attribute :>> parent = "H.TRAP.5";
            attribute :>> unit = ("arch::aarch64::signal::restore_signal_frame",
                "arch::aarch64::signal::find_fpsimd");
        }
    }

    package Context {
        doc /* src/kernel/src/arch/aarch64/switch.rs: moving between kernel stacks,
             * and a user task's floating-point, SIMD and thread-pointer state
             * across the switch, a fork and an execve. */

        requirement <'L.aarch64.11'> stacksSwitch : ItemLowLevel {
            attribute :>> statement = "switch_to shall save the callee-saved
                registers and the stack pointer of the running context and resume
                the one saved at next, and prepare_stack shall lay out a new stack
                so that the first switch to it calls entry with argument.";
            attribute :>> criterion = "Every task the scheduler starts runs its
                entry and every switch returns to the task switched from; the
                scheduler's checks assert the generic layer's fairness and wakes,
                and a check that a switch preserves x19 to x30 is not written.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("arch::aarch64::switch::switch_to",
                "arch::aarch64::switch::prepare_stack");
        }
        requirement <'L.aarch64.12'> userStateFollowsTheTask : ItemLowLevel {
            attribute :>> statement = "save_user_state and restore_user_state
                shall carry a user task's q0 to q31, FPCR, FPSR and TPIDR_EL0
                across a switch, capture shall copy them for a fork child, and
                reset_user_state and new shall give an execve the starting state:
                every register zero, round to nearest, no exception trapped, no
                thread pointer.";
            attribute :>> criterion = "Two tasks whose SIMD registers hold
                different values each read back their own after switches; a check
                of it is not written, though every musl program's memcpy would
                corrupt without it.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("arch::aarch64::switch::save_user_state",
                "arch::aarch64::switch::restore_user_state",
                "arch::aarch64::switch::reset_user_state",
                "arch::aarch64::switch::UserState::capture",
                "arch::aarch64::switch::UserState::new",
                "arch::aarch64::switch::UserState::set_thread_pointer",
                "arch::aarch64::switch::UserState::set_thread_area",
                "arch::aarch64::switch::UserState::fp_control",
                "arch::aarch64::switch::UserState::vectors",
                "arch::aarch64::switch::UserState::set_fp",
                "arch::aarch64::cpu::enable_user_fpu",
                "arch::aarch64::set_thread_area", "arch::aarch64::thread_area");
        }
    }

    package Interrupts {
        doc /* src/kernel/src/arch/aarch64/gic.rs, gic/gicv3.rs, gic/gicv3_its.rs
             * and the interrupt half of mod.rs: which controller the machine
             * has, a GICv3 and its ITS brought up, lines masked and let through,
             * and interrupts claimed, dispatched and retired. */

        requirement <'L.aarch64.13'> describedVersionPicksTheDriver : ItemLowLevel {
            attribute :>> statement = "described_version shall give a GIC the
                driver of the version firmware states, and for version 0 the one
                its register blocks imply -- a CPU interface GICv2's, a
                redistributor region GICv3's -- and none to a description with
                neither.";
            attribute :>> criterion = "Each of the machine check's 6 descriptions,
                versions 0, 2, 3 and 4 with and without each block, is given the
                driver listed for it.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = "arch::aarch64::gic::described_version";
        }
        requirement <'L.aarch64.14'> theControllerIsFoundAndBroughtUp : ItemLowLevel {
            attribute :>> statement = "init and init_from_tree shall find the
                interrupt controller in the MADT or the device tree and bring up
                the driver described_version chose, with its GICv2m frame or its
                ITS where the description has one, before any line is enabled.";
            attribute :>> criterion = "Every aarch64 boot the suite runs reports
                the controller it described, GICv2 by default and GICv3 with an
                ITS under boot-gicv3, and takes interrupts; the report is judged
                by test-boot's stage lines rather than by a check that asserts the
                version.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("arch::aarch64::gic::init", "arch::aarch64::gic::init_from_tree",
                "arch::aarch64::gic::init_v2", "arch::aarch64::gic::init_v3",
                "arch::aarch64::gic::layout", "arch::aarch64::gic::first_its",
                "arch::aarch64::gic::is_v3", "arch::aarch64::gic::init_this_cpu",
                "arch::aarch64::init_interrupts");
        }
        requirement <'L.aarch64.15'> gicv3IsConfigured : ItemLowLevel {
            attribute :>> statement = "gicv3::init shall map the distributor and
                the redistributor region, put every shared line off in group 1 at
                the default priority with affinity routing on, and init_this_cpu
                shall wake this core's redistributor, found by its affinity, and
                enable its CPU interface's system registers.";
            attribute :>> criterion = "The boot-gicv3 boot takes its timer and
                device interrupts on every core; a check that a shared line is off
                after bring-up and that each core found its own redistributor is
                not written.";
            attribute :>> parent = "H.IRQ.3";
            attribute :>> unit = ("arch::aarch64::gic::gicv3::init",
                "arch::aarch64::gic::gicv3::configure_distributor",
                "arch::aarch64::gic::gicv3::this_redistributor",
                "arch::aarch64::gic::gicv3::this_redistributor_both",
                "arch::aarch64::gic::gicv3::distributor_typer",
                "arch::aarch64::gic::gicv3::init_this_cpu",
                "arch::aarch64::gic::gicv3::window", "arch::aarch64::gic::gicv3::wait_for",
                "arch::aarch64::cpu::enable_gicv3_cpu_interface");
        }
        requirement <'L.aarch64.16'> linesAreMaskedAtTheController : ItemLowLevel {
            attribute :>> statement = "mask_interrupt and unmask_interrupt shall
                stop and let through a line at the controller, in the distributor
                for a shared line and this core's redistributor for a private one,
                so that the register reads back what was asked.";
            attribute :>> criterion = "An idle private line and an idle shared
                line, unmasked and masked again, each read back enabled and then
                disabled at the controller, on the GICv2 boot and the GICv3 one.";
            attribute :>> parent = "H.IRQ.2";
            attribute :>> unit = ("arch::aarch64::mask_interrupt", "arch::aarch64::unmask_interrupt",
                "arch::aarch64::gic::enable", "arch::aarch64::gic::disable",
                "arch::aarch64::gic::gicv3::enable", "arch::aarch64::gic::gicv3::disable",
                "arch::aarch64::gic::gicv3::registers_for");
        }
        requirement <'L.aarch64.17'> specialIdentifiersAreNoLines : ItemLowLevel {
            attribute :>> statement = "mask_interrupt and unmask_interrupt shall
                refuse the identifiers 1020 to 1023, which are the controller's
                answers and not lines.";
            attribute :>> criterion = "Masking 1020 and unmasking 1023 are both
                refused.";
            attribute :>> parent = "H.IRQ.2";
            attribute :>> unit = ("arch::aarch64::mask_interrupt", "arch::aarch64::unmask_interrupt");
        }
        requirement <'L.aarch64.18'> interruptsAreClaimedAndRetired : ItemLowLevel {
            attribute :>> statement = "service_interrupts shall claim every
                pending interrupt, dispatch it and retire it with exactly the value
                its claim returned, until the controller answers spurious.";
            attribute :>> criterion = "Every boot's timer ticks and device
                interrupts arrive; the generic interrupt checks assert delivery to
                the holder (H.IRQ.1), not the claim and retire order, and a check
                of that order is not written.";
            attribute :>> parent = ("H.IRQ.1", "H.IRQ.3");
            attribute :>> unit = ("arch::aarch64::service_interrupts",
                "arch::aarch64::gic::claim", "arch::aarch64::gic::complete",
                "arch::aarch64::gic::gicv3::claim", "arch::aarch64::gic::gicv3::complete",
                "arch::aarch64::cpu::read_icc_iar1", "arch::aarch64::cpu::write_icc_eoir1");
        }
        requirement <'L.aarch64.19'> processorInterruptsAreSent : ItemLowLevel {
            attribute :>> statement = "send_ipi_to_others and send_ipi_to shall
                interrupt the other processors on the IPI line after ordering the
                sender's stores before the interrupt, by the distributor on a
                GICv2 and ICC_SGI1R_EL1 on a GICv3.";
            attribute :>> criterion = "The shootdown check's flushes reach every
                processor (H.MEM.7, smp/check.rs); a check that an IPI arrives on
                each other core and on none twice is not written.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("arch::aarch64::send_ipi_to_others", "arch::aarch64::send_ipi_to",
                "arch::aarch64::ipi_irq", "arch::aarch64::gic::send_sgi_to_others",
                "arch::aarch64::gic::gicv3::send_sgi_to_others",
                "arch::aarch64::cpu::write_icc_sgi1r", "arch::aarch64::cpu::dsb_ishst");
        }
        requirement <'L.aarch64.20'> itsTranslatesMessages : ItemLowLevel {
            attribute :>> statement = "The ITS driver shall quiesce an ITS
                firmware left on, give it zeroed device and collection tables, a
                command queue and each core's LPI property and pending tables,
                and map a device's next event to a free LPI collected on the boot
                core, waiting for each command to be consumed.";
            attribute :>> criterion = "Under boot-gicv3 the virtio devices'
                message interrupts arrive through the ITS; a check that a mapped
                event raises its LPI and only it is not written.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("arch::aarch64::gic::gicv3_its::init",
                "arch::aarch64::gic::gicv3_its::bring_up",
                "arch::aarch64::gic::gicv3_its::enable_lpis",
                "arch::aarch64::gic::gicv3_its::pending_table",
                "arch::aarch64::gic::gicv3_its::set_attributes",
                "arch::aarch64::gic::gicv3_its::quiesce",
                "arch::aarch64::gic::gicv3_its::command_queue",
                "arch::aarch64::gic::gicv3_its::program_tables",
                "arch::aarch64::gic::gicv3_its::program_table",
                "arch::aarch64::gic::gicv3_its::allocate",
                "arch::aarch64::gic::gicv3_its::number",
                "arch::aarch64::gic::gicv3_its::Its::free_event",
                "arch::aarch64::gic::gicv3_its::Its::take",
                "arch::aarch64::gic::gicv3_its::Its::map_device",
                "arch::aarch64::gic::gicv3_its::Its::enable_property",
                "arch::aarch64::gic::gicv3_its::Its::submit",
                "arch::aarch64::gic::gicv3_its::zeroed",
                "arch::aarch64::gic::gicv3_its::order_for",
                "arch::aarch64::gic::gicv3_its::read64",
                "arch::aarch64::gic::gicv3_its::write64",
                "arch::aarch64::gic::msi_allocate", "arch::aarch64::msi_allocate");
        }
        requirement <'L.aarch64.21'> itsRefusesAnUnknownDevice : ItemLowLevel {
            attribute :>> statement = "gicv3_its::allocate shall refuse a device
                ID past the device table the ITS was given, rather than map it.";
            attribute :>> criterion = "On the GICv3 boot with an ITS, asking for a
                vector for device ID u32::MAX is refused.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = "arch::aarch64::gic::gicv3_its::allocate";
        }
        requirement <'L.aarch64.22'> msiDoorbellIsNamed : ItemLowLevel {
            attribute :>> statement = "msi_doorbell shall name the page a
                device's message writes land in -- the ITS's translater or the
                GICv2m frame -- so that an IOMMU domain maps it, and none on a
                machine with neither.";
            attribute :>> criterion = "The iommu domain check's device writes its
                message through its domain (L.iommu); a check that the page named
                is the one the controller decodes is not written.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("arch::aarch64::msi_doorbell", "arch::aarch64::gic::msi_doorbell",
                "arch::aarch64::gic::gicv3_its::doorbell");
        }
        requirement <'L.aarch64.23'> interruptMaskFollowsTheCaller : ItemLowLevel {
            attribute :>> statement = "disable_interrupts shall mask debug,
                SError, IRQ and FIQ, enable_interrupts shall unmask IRQ alone, a
                saved DAIF shall be put back exactly by Irq::restore, and
                wait_for_work shall wait with IRQ masked and then unmask it, so
                that an interrupt pending before the wait ends it.";
            attribute :>> criterion = "Every lock that masks interrupts restores
                the state it found; a check that a nested save and restore leaves
                DAIF as it was is not written.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("arch::aarch64::enable_interrupts", "arch::aarch64::disable_interrupts",
                "arch::aarch64::wait_for_work", "arch::aarch64::wait_for_interrupt",
                "arch::aarch64::interrupts_enabled", "arch::aarch64::Irq::disable",
                "arch::aarch64::Irq::restore", "arch::aarch64::cpu::wfi",
                "arch::aarch64::cpu::wait_then_enable_interrupts",
                "arch::aarch64::cpu::disable_interrupts", "arch::aarch64::cpu::enable_interrupts",
                "arch::aarch64::cpu::read_daif", "arch::aarch64::cpu::write_daif",
                "arch::aarch64::halt");
        }
    }

    package Timer {
        doc /* src/kernel/src/arch/aarch64/timer.rs and its wrappers in mod.rs: the
             * generic timer's virtual counter and comparator. */

        requirement <'L.aarch64.24'> theTimerFiresOnce : ItemLowLevel {
            attribute :>> statement = "timer::arm shall make the virtual timer
                fire once, nanos from now, by setting its comparator to the
                counter plus nanos in counter ticks and enabling it, and disarm
                shall mask it.";
            attribute :>> criterion = "Stage 3's timer check: a one-shot armed for
                a millisecond fires, and has not fired again ten intervals later;
                then 250 ticks at a requested 1,000 Hz take a time the counter
                measures as within 25 per cent of that rate, every tick an
                interrupt a handler took.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::aarch64::timer::arm", "arch::aarch64::timer::disarm",
                "arch::aarch64::timer_arm", "arch::aarch64::timer_disarm",
                "arch::aarch64::cpu::write_cntv_cval", "arch::aarch64::cpu::write_cntv_ctl");
        }
        requirement <'L.aarch64.25'> theCounterRuns : ItemLowLevel {
            attribute :>> statement = "timer::init and init_from_tree shall take
                the timer's interrupt as firmware describes it, a private one, and
                shall refuse a counter whose CNTFRQ_EL0 reads zero or that does not
                advance; counter_now shall read the counter after an isb.";
            attribute :>> criterion = "Every boot reports the counter's frequency
                and ticks; a check that a zero frequency is refused is not
                written.";
            attribute :>> parent = "H.SCHED.5";
            attribute :>> unit = ("arch::aarch64::timer::init", "arch::aarch64::timer::init_from_tree",
                "arch::aarch64::timer::finish", "arch::aarch64::timer::irq",
                "arch::aarch64::timer::counter_now", "arch::aarch64::timer::counter_hz",
                "arch::aarch64::timer_irq", "arch::aarch64::counter_now",
                "arch::aarch64::counter_hz", "arch::aarch64::cpu::read_cntfrq",
                "arch::aarch64::cpu::read_cntvct");
        }
        requirement <'L.aarch64.26'> programsReadTheCounter : ItemLowLevel {
            attribute :>> statement = "allow_user_counter shall let EL0 read the
                virtual counter and CNTFRQ_EL0, as Linux does, and nothing else of
                the timer.";
            attribute :>> criterion = "A program's clock_gettime through musl
                reads the counter; a check that EL0 can read CNTVCT_EL0 and cannot
                write the comparator is not written.";
            attribute :>> parent = "H.MEM.2";
            attribute :>> unit = ("arch::aarch64::cpu::allow_user_counter",
                "arch::aarch64::vdso_spec", "arch::aarch64::vdso_can_read_counter");
        }
    }

    package Console {
        doc /* src/kernel/src/arch/aarch64/console.rs and its wrappers in mod.rs:
             * which console the command line names -- the PL011
             * (arch/arm_common), a 16550 at an MMIO address, or a ramoops
             * record -- and bytes to and from it. */

        requirement <'L.aarch64.27'> a16550ValueIsReadOrRefused : ItemLowLevel {
            attribute :>> statement = "ns16550_port shall read
                `uart8250,mmio,<address>` as a 16550 at that address only when the
                address is hexadecimal with a 0x prefix, and shall refuse a port
                in I/O space and any other console's value. The ramoops zone's
                value is console's (L.console.40).";
            attribute :>> criterion = "`uart8250,mmio,0x3f8` is read as the port at
                0x3f8; `uart8250,mmio,3f8`, `uart8250,mmio,0x`, `uart8250,io,0x3f8`
                and a ramoops value are each refused.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = "arch::aarch64::console::ns16550_port";
        }
        requirement <'L.aarch64.28'> theNamedConsoleIsMapped : ItemLowLevel {
            attribute :>> statement = "console::init shall map the console the
                command line names as device memory before the first byte: a
                16550's page at its address, a ramoops zone whole, continuing the
                loader's record where its signature is, or else the PL011.";
            attribute :>> criterion = "QEMU's boots print through the PL011; the
                crosvm guest's log (~/.local/share/ferrix/pixel7/vm-20260926-140648)
                through its 16550; the Pixel 7's boot log through its ramoops zone
                after the watchdog reset. None of the three is a check.";
            attribute :>> parent = ("H.BOOT.8", "H.FAIL.1");
            attribute :>> unit = ("arch::aarch64::console::init", "arch::aarch64::init_console",
                "arch::aarch64::console::is_ns16550", "arch::aarch64::console::is_ramoops");
        }
        requirement <'L.aarch64.29'> bytesReachTheConsole : ItemLowLevel {
            attribute :>> statement = "write_byte and put shall hand a byte to
                the console the command line named -- the PL011, the 16550 once its
                holding register has room within a bounded wait, or the ramoops
                record, dropping it once the zone is full -- and drain shall wait,
                bounded, until the port has sent everything.";
            attribute :>> criterion = "Every gate reads the boot's log from the
                serial port, FERRIX-BOOT-OK among it; the 16550 and ramoops arms
                run only on the crosvm guest and the Pixel 7.";
            attribute :>> parent = ("H.BOOT.8", "H.FAIL.1");
            attribute :>> unit = ("arch::aarch64::console::write_byte", "arch::aarch64::console::put",
                "arch::aarch64::console::drain", "arch::aarch64::console::transmit_room",
                "arch::aarch64::console::transmit_interrupt",
                "arch::aarch64::console::transmit_buffer",
                "arch::aarch64::console::ramoops_append", "arch::aarch64::console::ramoops_read",
                "arch::aarch64::console::ramoops_write", "arch::aarch64::console::read_u8",
                "arch::aarch64::console::write_u8", "arch::aarch64::drain_console");
        }
        requirement <'L.aarch64.30'> consoleInputArrives : ItemLowLevel {
            attribute :>> statement = "read_byte shall give a byte the port
                received, none from a ramoops record, and the receive interrupt
                shall be enabled at the controller and in the port only for the
                PL011, whose bytes read_console_byte then takes from the receive
                ring.";
            attribute :>> criterion = "The console input check runs the ring
                (console/input.rs); a check that a byte the host sends reaches a
                reader on this architecture is test-shell's typing, not a
                check.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("arch::aarch64::console::read_byte",
                "arch::aarch64::console::enable_receive_interrupt",
                "arch::aarch64::read_console_byte", "arch::aarch64::take_console_byte",
                "arch::aarch64::console_receive_irq", "arch::aarch64::enable_console_receive");
        }
    }

    package Translation {
        doc /* The translation half of mod.rs and cpu.rs: the loader's identity
             * map dropped, a program's tables installed in TTBR0_EL1 beside the
             * kernel's in TTBR1_EL1, the TLB invalidated, and the cache
             * maintenance code and shared memory need. */

        requirement <'L.aarch64.31'> userRootsAreInstalled : ItemLowLevel {
            attribute :>> statement = "install_user_root shall translate the lower
                half through a program's root in TTBR0_EL1 and drop this
                processor's cached user translations, and uninstall_user_root
                shall stop translating the lower half, so that a kernel thread
                reaches no program's memory.";
            attribute :>> criterion = "Two programs mapping one address reach
                their own frames (H.MEM.1, checked generically); a check that a
                kernel thread's lower-half access faults is not written.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("arch::aarch64::install_user_root",
                "arch::aarch64::uninstall_user_root", "arch::aarch64::prepare_user_root",
                "arch::aarch64::cpu::write_ttbr0", "arch::aarch64::cpu::disable_ttbr0",
                "arch::aarch64::cpu::flush_user_tlb");
        }
        requirement <'L.aarch64.32'> theIdentityMapGoes : ItemLowLevel {
            attribute :>> statement = "drop_identity_map shall stop the lower
                half translating through the loader's identity map, and
                identity_map_live shall read from TCR_EL1 and TTBR0_EL1 whether it
                still does.";
            attribute :>> criterion = "Stage 1 requires identity_map_live false
                after the drop; it is in main.rs, product code.";
            attribute :>> parent = "H.BOOT.1";
            attribute :>> unit = ("arch::aarch64::drop_identity_map",
                "arch::aarch64::identity_map_live", "arch::aarch64::identity_root",
                "arch::aarch64::kernel_write_protected", "arch::aarch64::cpu::read_tcr",
                "arch::aarch64::cpu::read_ttbr0", "arch::aarch64::cpu::read_mair",
                "arch::aarch64::cpu::read_sctlr");
        }
        requirement <'L.aarch64.33'> tablesArePublishedAndFlushed : ItemLowLevel {
            attribute :>> statement = "flush_tlb and flush_tlb_page shall make
                every table write before them visible to the walker (dsb ishst)
                and then invalidate the whole TLB, or the page's entries for every
                ASID, inner-shareable, so that every core drops them.";
            attribute :>> criterion = "The shootdown checks (smp/check.rs) assert
                that a table waits for its shootdown and that a flushed
                translation is gone on every processor, across architectures;
                this architecture's instructions are not asserted apart.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("arch::aarch64::flush_tlb", "arch::aarch64::flush_tlb_page",
                "arch::aarch64::cpu::flush_tlb", "arch::aarch64::cpu::flush_tlb_page");
        }
        requirement <'L.aarch64.34'> cachesAreMaintained : ItemLowLevel {
            attribute :>> statement = "clean_to_poc and clean_invalidate_to_poc
                shall write back, and drop, every data cache line covering a range
                by the smallest line CTR_EL0 gives, and sync_instructions shall
                make instructions written through the data side the ones every
                core fetches.";
            attribute :>> criterion = "A secondary that starts with its caches off
                reads the start block the boot core wrote, and a program's page
                of code runs as written; on QEMU, which models no caches, neither
                can fail, and a real core's evidence is the board's boot.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = ("arch::aarch64::cpu::clean_to_poc",
                "arch::aarch64::cpu::clean_invalidate_to_poc",
                "arch::aarch64::cpu::sync_instructions", "arch::aarch64::cpu::data_line");
        }
        requirement <'L.aarch64.35'> panKeepsTheKernelOut : ItemLowLevel {
            attribute :>> statement = "enable_user_access_protection shall turn
                on PAN where the core implements it, and permit_user_access and
                forbid_user_access shall clear and set PSTATE.PAN around a user
                copy alone.";
            attribute :>> criterion = "The boot line says PAN on under QEMU's
                max CPU; a check that a kernel read of a user page outside a copy
                faults is not written.";
            attribute :>> parent = "H.MEM.3";
            attribute :>> unit = ("arch::aarch64::cpu::enable_user_access_protection",
                "arch::aarch64::cpu::permit_user_access", "arch::aarch64::cpu::forbid_user_access",
                "arch::aarch64::permit_user_access", "arch::aarch64::forbid_user_access");
        }
        requirement <'L.aarch64.49'> devicesSeeAccessesInOrder : ItemLowLevel {
            attribute :>> statement = "dma_barrier shall order every memory access
                before it against every one after it as a DMA master observes them,
                both ways -- dmb osh, the outer-shareable domain a device sits in --
                so that the item's virtqueue publishes a descriptor before the index
                that counts it, reads a used entry only after the index that counts
                it, and rings the doorbell only after the index is published.";
            attribute :>> criterion = "No check can fail without it on the machines
                the suite boots: QEMU's TCG makes a core's accesses in program
                order. The instruction is argued (F-44). A real core's evidence is
                the Pixel 7's crosvm guest running stage 10's virtio check, when
                the hardware may be used (F-44's BACKLOG row): the DK1 has no PCI
                and no virtio. H.DMA.2 is its parent because the item's virtqueue
                exists only to drive that requirement's out-of-domain probe.";
            attribute :>> parent = "H.DMA.2";
            attribute :>> unit = "arch::aarch64::cpu::dma_barrier";
        }
    }

    package Processors {
        doc /* src/kernel/src/arch/aarch64/smp.rs and the processor half of
             * cpu.rs: which processors firmware says can be started, starting
             * them through PSCI, and what each core says it has. */

        requirement <'L.aarch64.36'> processorsAreDescribed : ItemLowLevel {
            attribute :>> statement = "describe_cpus shall name every processor
                the MADT's enabled GIC CPU interfaces, or the device tree's /cpus
                with PSCI as its enable method, describe, by affinity, with this
                one as the boot processor, and the boot processor alone under
                nosmp.";
            attribute :>> criterion = "The boot line counts the cores QEMU was
                given (4, and 1 under boot-nosmp); it is judged by test-boot's
                lines, not a check.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::aarch64::smp::describe_cpus",
                "arch::aarch64::smp::cpus_from_madt", "arch::aarch64::smp::cpus_from_tree",
                "arch::aarch64::smp::hardware_id", "arch::aarch64::cpu::read_mpidr");
        }
        requirement <'L.aarch64.37'> secondariesStart : ItemLowLevel {
            attribute :>> statement = "CpuStarter shall start a processor through
                PSCI CPU_ON on its own stack with an identity map and a start
                block whose registers are the boot core's, secondary_start shall
                bring it into the kernel, and finish shall take the identity map
                down only after every started core has reported in.";
            attribute :>> criterion = "Every core described reports in; the
                count is judged by stage 4's line in main.rs, product code.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("arch::aarch64::smp::CpuStarter::new",
                "arch::aarch64::smp::CpuStarter::start", "arch::aarch64::smp::CpuStarter::finish",
                "arch::aarch64::smp::secondary_start", "arch::aarch64::smp::psci",
                "arch::aarch64::smp::psci_conduit", "arch::aarch64::cpu::hvc_call",
                "arch::aarch64::cpu::smc_call");
        }
        requirement <'L.aarch64.38'> programsSeeTheCoresFeatures : ItemLowLevel {
            attribute :>> statement = "user_hwcaps shall give a program the
                AT_HWCAP and AT_HWCAP2 bits arch/arm64 gives for what
                ID_AA64PFR0_EL1, ID_AA64ISAR0_EL1 and ID_AA64ISAR1_EL1 say the core
                has, and user_platform none.";
            attribute :>> criterion = "The mapping is ferrix-linux-abi's hwcap,
                host-tested against Linux's bits; that the registers read here
                reach it is not checked on the machine.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("arch::aarch64::user_hwcaps", "arch::aarch64::user_platform",
                "arch::aarch64::cpu::read_id_aa64pfr0", "arch::aarch64::cpu::read_id_aa64isar0",
                "arch::aarch64::cpu::read_id_aa64isar1");
        }
    }

    package Speculation {
        doc /* src/kernel/src/arch/aarch64/speculation.rs: what each core needs
             * against Spectre v2, Spectre-BHB, Meltdown and speculative store
             * bypass, from its own identification registers and Arm's lists,
             * what firmware is asked, what is applied and read back, and the
             * index clamp at the system call boundary. */

        requirement <'L.aarch64.39'> partTablesAreLinuxs : ItemLowLevel {
            attribute :>> statement = "A core shall be given the Spectre-BHB loop
                count, the Spectre v2 safe listing, the Meltdown safe listing and
                the in-order classification Linux's proton-pack.c and cpufeature.c
                give its part, none of Arm's listings to a core another company
                designed, and no loop to a core whose branch history is not
                shared.";
            attribute :>> criterion = "Each of the check's 12 cores -- ten of
                Arm's parts, one with ECBHB, one of Qualcomm's -- is given the
                figures listed for it, and the ECBHB core is reported unaffected
                by Spectre-BHB.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::aarch64::speculation::Core::read",
                "arch::aarch64::speculation::Core::arm_part",
                "arch::aarch64::speculation::Core::v2_listed_safe",
                "arch::aarch64::speculation::Core::v2_unaffected",
                "arch::aarch64::speculation::Core::bhb_loops",
                "arch::aarch64::speculation::Core::meltdown_safe",
                "arch::aarch64::speculation::Core::in_order",
                "arch::aarch64::speculation::Exposure::v2",
                "arch::aarch64::speculation::Exposure::bypass",
                "arch::aarch64::speculation::Exposure::fmt",
                "arch::aarch64::speculation::CoreName::fmt",
                "arch::aarch64::speculation::read_midr",
                "arch::aarch64::speculation::read_id_aa64pfr1",
                "arch::aarch64::speculation::read_id_aa64mmfr1");
        }
        requirement <'L.aarch64.40'> firmwareAnswersAreRead : ItemLowLevel {
            attribute :>> statement = "An SMCCC_ARCH_FEATURES status shall be
                read as ARM DEN 0028D defines it -- 0 required, 1 not required, -2
                always on, anything else not supported -- and shall survive the
                record it is packed into.";
            attribute :>> criterion = "Each of the five statuses the check tries
                is read as listed and comes back from its bits unchanged.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::aarch64::speculation::Answer::from_status",
                "arch::aarch64::speculation::Answer::bits",
                "arch::aarch64::speculation::Answer::from_bits");
        }
        requirement <'L.aarch64.41'> theEntryLoopMatchesTheRecords : ItemLowLevel {
            attribute :>> statement = "Each processor shall decide its defences
                from its core and firmware's answers, apply them, record what it
                saw and applied, and the vector entry's branch history loop shall
                run exactly when some processor recorded that it needs it.";
            attribute :>> criterion = "After every processor has started, the
                loop's count is non-zero exactly when a processor's record holds
                the loop.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::aarch64::speculation::init",
                "arch::aarch64::speculation::apply_this_cpu",
                "arch::aarch64::speculation::decide_and_apply",
                "arch::aarch64::speculation::plan_for",
                "arch::aarch64::speculation::Seen::pack",
                "arch::aarch64::speculation::Seen::unpack",
                "arch::aarch64::speculation::kind_of", "arch::aarch64::init_speculation");
        }
        requirement <'L.aarch64.42'> defencesAreAppliedAndReadBack : ItemLowLevel {
            attribute :>> statement = "apply shall clear SCTLR_EL1.DSSBS and
                PSTATE.SSBS on a core that needs the store bypass defence and has
                them, read back what can be read, and say what took; firmware's
                workaround 1 shall be asked on each switch only by a core whose
                plan holds it.";
            attribute :>> criterion = "The speculation report names each kind of
                core and what was applied; QEMU's max CPU needs none of them, and a
                check that a cleared DSSBS reads back cleared on a core that has
                it is not written.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::aarch64::speculation::apply",
                "arch::aarch64::speculation::write_sctlr",
                "arch::aarch64::speculation::clear_ssbs",
                "arch::aarch64::speculation::switch_barrier",
                "arch::aarch64::speculation::answers_arch_features",
                "arch::aarch64::speculation::ask_about_this_core",
                "arch::aarch64::speculation::firmware_call",
                "arch::aarch64::speculation::report_once_started");
        }
        requirement <'L.aarch64.43'> indicesAreClamped : ItemLowLevel {
            attribute :>> statement = "clamp_index and clamp_below shall answer
                an index below its bound unchanged and any other as zero, with a
                csel and a csdb so that no later instruction uses a predicted
                result.";
            attribute :>> criterion = "The generic speculation check requires the
                clamp on every architecture (arch/speculation_check.rs), whose
                criterion is the clamp's result, not this instruction sequence.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = ("arch::aarch64::speculation::clamp_index",
                "arch::aarch64::speculation::clamp_below");
        }
    }

    package Firmware {
        doc /* What firmware is asked for: power off and reset through PSCI,
             * the Pixel 7's watchdogs fed, and entropy from its TRNG through
             * SMCCC (trng.rs) or the core's RNDR. */

        requirement <'L.aarch64.44'> trngWordsAreTakenInOrder : ItemLowLevel {
            attribute :>> statement = "fill_from shall fill its buffer from
                TRNG_RND64 192 bits at a time, each call's x3 then x2 then x1 as
                little-endian bytes, asking again at most ATTEMPTS times while
                firmware reports none ready, and shall stop at the bytes already
                taken when a call fails.";
            attribute :>> criterion = "Against scripted firmware: 40 bytes are
                taken over three calls, the first finding nothing ready, in that
                order; a failure on the second call leaves 24 bytes taken and the
                rest untouched; a firmware never ready is asked 8 times and gives
                0 bytes.";
            attribute :>> parent = "H.BOOT.3";
            attribute :>> unit = ("arch::aarch64::trng::fill_from", "arch::aarch64::trng::rnd64");
        }
        requirement <'L.aarch64.45'> trngIsAskedOnlyWhereOffered : ItemLowLevel {
            attribute :>> statement = "offers_rnd64 shall ask TRNG_RND64 only of
                firmware that reports PSCI 1.0 or later, answers SMCCC_VERSION
                through PSCI_FEATURES, reports SMCCC 1.1 or later and TRNG 1.0 or
                later, and offers TRNG_RND64 through TRNG_FEATURES.";
            attribute :>> criterion = "Five scripted firmwares -- PSCI 0.2, no
                SMCCC, SMCCC 1.0, no TRNG, no TRNG_RND64 -- each give 0 bytes and
                are asked no TRNG_RND64.";
            attribute :>> parent = "H.BOOT.3";
            attribute :>> unit = "arch::aarch64::trng::offers_rnd64";
        }
        requirement <'L.aarch64.46'> theSeedAsksFirmware : ItemLowLevel {
            attribute :>> statement = "firmware_entropy shall ask the TRNG through
                the conduit the machine describes PSCI on, and give 0 bytes where
                it describes none; hardware_random shall give a word only from a
                core ID_AA64ISAR0_EL1 says has RNDR, and only one RNDR reports
                valid within ten tries.";
            attribute :>> criterion = "The crosvm guest's boot seeds 48 bytes from
                its TRNG (~/.local/share/ferrix/pixel7/vm-20260926-140648) and
                QEMU's 0; neither is a check, and RNDR is not asserted.";
            attribute :>> parent = "H.BOOT.3";
            attribute :>> unit = ("arch::aarch64::firmware_entropy", "arch::aarch64::trng::fill",
                "arch::aarch64::trng::call", "arch::aarch64::cpu::hvc_call_x0_x3",
                "arch::aarch64::cpu::smc_call_x0_x3", "arch::aarch64::cpu::hardware_random");
        }
        requirement <'L.aarch64.47'> theMachineStops : ItemLowLevel {
            attribute :>> statement = "shutdown and reset shall drain the console
                and then ask PSCI for SYSTEM_OFF or SYSTEM_RESET, powering off
                where reset returns and halting where power-off returns.";
            attribute :>> criterion = "Every gate's QEMU exits when the boot
                powers off, and boot-reset's ferrix.onexit=reset is x86-64's; a
                reset on this architecture is not asserted.";
            attribute :>> parent = ("H.BOOT.6", "H.BOOT.7");
            attribute :>> unit = ("arch::aarch64::shutdown", "arch::aarch64::reset",
                "arch::aarch64::cpu::psci_system_off", "arch::aarch64::cpu::psci_system_reset",
                "arch::aarch64::cpu::psci_system");
        }
        requirement <'L.aarch64.48'> watchdogsAreFed : ItemLowLevel {
            attribute :>> statement = "init_watchdogs shall feed once every
                watchdog the device tree lists that firmware left running, and
                start_watchdogs shall start the task that keeps feeding them, and
                nothing where the tree lists none.";
            attribute :>> criterion = "The Pixel 7 runs past its watchdogs'
                timeout; QEMU's tree lists none. Not a check.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("arch::aarch64::init_watchdogs", "arch::aarch64::start_watchdogs");
        }
        requirement <'L.aarch64.50'> registerAccessesAreEmulable : ItemLowLevel {
            attribute :>> statement = "mmio's read8, read16 and read32 shall each
                be one ldrb, ldrh or ldr, and write8, write16 and write32 one strb,
                strh or str, with a bare base register and no writeback, so that a
                hypervisor can emulate the access from its fault's syndrome.";
            attribute :>> criterion = "No check can fail without it on the machines
                the suite boots: QEMU's TCG emulates any load or store. The Pixel
                7's crosvm guest is the evidence: with read_volatile, LLVM made
                stage 10's DMA switch check a pre-indexed ldrh, KVM_RUN failed
                with ENOSYS and the guest stopped with neither boot marker; with
                these, it boots to init.";
            attribute :>> parent = "H.BOOT.2";
            attribute :>> unit = ("arch::aarch64::mmio::read8", "arch::aarch64::mmio::write8",
                "arch::aarch64::mmio::read16", "arch::aarch64::mmio::write16",
                "arch::aarch64::mmio::read32", "arch::aarch64::mmio::write32");
        }
        requirement <'L.aarch64.51'> theFilterIsAskedFirstAtTheSvc : ItemLowLevel {
            attribute :>> statement = "system_call shall ask the registered filter
                about a call before rt_sigreturn is answered and before the
                dispatcher is, with x8's low 32 bits, ELR as the instruction after the
                svc and AArch64's token, apply the filter's errno as the dispatcher's answer,
                and syscall_rollback_value shall give x0 the call's first argument so
                that a call that did not run reads as it did at the call.";
            attribute :>> criterion = "A test-only filter is shown rt_sigreturn once,
                before its answer, and its value comes back in x0 (the `seccomp`
                line).";
            attribute :>> parent = "H.TRAP.16";
            attribute :>> unit = ("arch::aarch64::audit_arch",
                "arch::aarch64::syscall_rollback_value");
        }
    }
}
