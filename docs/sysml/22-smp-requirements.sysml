/*
 * Ferrix — SysML v2 model, part 22: the low-level requirements of smp.
 *
 * What each unit of src/kernel/src/smp.rs does, as `ItemLowLevel` requirements
 * (part 13 defines the format, part 14 is the pilot this copies): finding
 * the processors and giving each a record it finds itself by, starting the
 * secondaries, the inter-processor interrupt, the TLB shootdown -- whole and
 * scoped, and the bound on how long it waits -- grace periods, stopping the
 * other processors for a panic, and the scheduler's kick. The start
 * sequences themselves, the per-processor register and the interrupt
 * controller are each architecture's (src/kernel/src/arch/<isa>/smp.rs), and
 * belong to the arch slices; this is the architecture-independent half
 * above them.
 *
 * The pilot's rules hold: a requirement names every function that carries
 * its behaviour as its `unit`; one check proves a whole requirement, so a
 * requirement is no broader than one check can prove; a statement says no
 * more than its criterion tests; an accessor needs no requirement of its own;
 * check code living in a product file is listed in
 * tools/common/data/traceability-units.json, not required -- here
 * `run_everywhere` and `next_job`, the way stage 4's checks hand work to
 * every processor, which nothing else uses.
 *
 * What proves what. Stage 4's checks in smp/check.rs run on every processor
 * at once: `everywhere` (100 rounds of work, each processor's record against
 * its hardware, the interrupts the secondaries took), `page_sets`,
 * `tables_wait_for_their_shootdown`, `shootdown` (a page moved under every
 * processor 20 times), `grace`, and after the scheduler starts
 * `migrating_shootdown`. The scoped shootdown reaching another processor is
 * proved from user mode by user/rmap_check.rs's protect step. The bound on a
 * wait for a processor that never answers is proved only by the negative
 * control recorded in commit 94dee288, a scratch edit that made one
 * processor stop answering, and the bound on the shootdown turn by nothing
 * at all (commit 237d2426 says why): no boot check makes a processor stuck,
 * because the boot test counts the panic that follows as a failure. They
 * wait in the baseline with the rest of what no check proves;
 * docs/certification/IMPLEMENTATION.md W-8 lists it.
 *
 * Ids are `L.smp.<n>`, numbered through the subsystem; the package gives the
 * part of it. They are never renumbered once a check names them.
 */
package FerrixSmpRequirements {
    private import FerrixItemRequirements::*;

    package Processors {
        doc /* The processors firmware describes, the record each is given,
             * and how code finds the record of the processor it runs on. */

        requirement <'L.smp.1'> impossibleListsAreRefused : ItemLowLevel {
            attribute :>> statement = "Topology::from_described shall refuse a
                processor list that is empty, that names one hardware identifier
                twice, or that does not name the processor running it.";
            attribute :>> criterion = "Described lists that are empty, that hold one
                identifier twice and that omit the boot processor's identifier are
                each refused with its own error, 0 accepted; a list of 4 distinct
                identifiers including the boot one is accepted.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("smp::Topology::from_described", "smp::discover");
        }
        requirement <'L.smp.2'> theBootProcessorIsZero : ItemLowLevel {
            attribute :>> statement = "Topology::from_described shall give the boot
                processor logical number 0 and the others 1 to N-1 in table order,
                one record each that holds its own address, with only the boot
                processor's record online.";
            attribute :>> criterion = "A described list of 4 identifiers whose boot
                identifier is third yields 4 records, the boot identifier's at index
                0 and the other three at 1 to 3 in table order, each record's own
                address in its first word, and 1 online.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = "smp::Topology::from_described";
        }
        requirement <'L.smp.3'> eachProcessorFindsItsOwnRecord : ItemLowLevel {
            attribute :>> statement = "On every processor that has installed its
                record, this_cpu shall return the record that names that processor's
                hardware identifier.";
            attribute :>> criterion = "In 100 rounds of work run on every online
                processor at once, each processor runs each round once and the record
                this_cpu gives it names its own hardware identifier every time: 0
                misplaced runs (the `smp` line).";
            attribute :>> parent = "H.SCHED.6";
            attribute :>> unit = ("smp::this_cpu", "smp::discover",
                "smp::install_secondary_record");
        }
        requirement <'L.smp.4'> noRecordBeforeItIsInstalled : ItemLowLevel {
            attribute :>> statement = "this_cpu shall return nothing until the boot
                processor has installed its record, and shall not read the
                per-processor register before then.";
            attribute :>> criterion = "Called before discover, this_cpu returns None
                with the register holding a value that is no record's address; after
                discover it returns the boot processor's record.";
            attribute :>> parent = "H.SCHED.6";
            attribute :>> unit = "smp::this_cpu";
        }
        requirement <'L.smp.5'> aWrongRecordHalts : ItemLowLevel {
            attribute :>> statement = "discover and secondary_main shall halt a
                processor whose register does not lead to its own record, whose
                record does not hold its own address or lead back to itself by its
                logical number, or names another processor's hardware identifier; and
                a secondary that arrives with an address that is no processor's
                record shall halt.";
            attribute :>> criterion = "A secondary started with another processor's
                record address, and one started with an address that is no record's,
                each end the boot in their catalogued panic (FX-0405, FX-0404); a
                boot processor record made to name another hardware identifier ends
                the boot in FX-0401.";
            attribute :>> parent = ("H.SCHED.6", "H.FAIL.1");
            attribute :>> unit = ("smp::check_this_cpu", "smp::discover",
                "smp::secondary_main", "smp::install_secondary_record");
        }
        requirement <'L.smp.6'> aReportNamesOnlyARealRecord : ItemLowLevel {
            attribute :>> statement = "this_cpu_for_report shall name the processor
                whose record's address the register holds, and otherwise say that the
                processor has not installed a record or that no other processor was
                started, without following the register.";
            attribute :>> criterion = "With the register holding a processor's record
                address the report names that processor; holding any other value, it
                answers with the no-record sentence; before discover, with the
                boot-processor sentence; 0 reads through the register's value.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "smp::this_cpu_for_report";
        }
    }

    package BringUp {
        doc /* Starting the secondaries, and what each does as it arrives: the
             * handshakes that let a shootdown or a grace period racing its
             * arrival cover it, and the wait for work until the scheduler
             * takes the processor over. */

        requirement <'L.smp.7'> everySecondaryComesUp : ItemLowLevel {
            attribute :>> statement = "start_secondaries shall start every processor
                the topology lists but the boot one, one at a time, each on a kernel
                stack of its own, and shall return only once each has reported
                itself online.";
            attribute :>> criterion = "On each configuration `cargo xtask test-boot`
                boots, the `cpus` line counts as many processors online as firmware
                described (4 of 4 on x86-64 and AArch64, 2 of 2 on ARMv7-A at `--smp
                2`).";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("smp::start_secondaries", "smp::wait_until_online",
                "smp::secondary_main");
        }
        requirement <'L.smp.8'> aSilentSecondaryIsAnError : ItemLowLevel {
            attribute :>> statement = "start_secondaries shall fail, and leave the
                architecture's start block in place, when a processor it started has
                not reported itself online within one second.";
            attribute :>> criterion = "A secondary made never to set its online flag
                ends the boot in FX-0402 with the never-reported-in sentence 1 s or
                more after it was started, and the start block is not freed.";
            attribute :>> parent = "H.BOOT.5";
            attribute :>> unit = ("smp::start_secondaries", "smp::wait_until_online");
        }
        requirement <'L.smp.9'> speculationIsDecidedFirst : ItemLowLevel {
            attribute :>> statement = "start_secondaries shall apply the side-channel
                defences on the boot processor before it starts any other
                processor, on a machine with one processor as well.";
            attribute :>> criterion = "The boot processor's speculation controls read
                back as decided before the first secondary's start is requested, on
                each architecture, and with `--smp 1`.";
            attribute :>> parent = "H.TRAP.3";
            attribute :>> unit = "smp::start_secondaries";
        }
        requirement <'L.smp.10'> theInterruptIsHeldFirst : ItemLowLevel {
            attribute :>> statement = "start_secondaries shall register the
                inter-processor interrupt's handler before any secondary starts, and
                shall fail if another handler holds its line.";
            attribute :>> criterion = "0 inter-processor interrupts are counted
                unclaimed across a boot; with the line registered beforehand,
                start_secondaries fails with the line-taken sentence and starts no
                processor.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = "smp::start_secondaries";
        }
        requirement <'L.smp.11'> anInterruptWakesASecondary : ItemLowLevel {
            attribute :>> statement = "A secondary waiting in secondary_main shall
                take an inter-processor interrupt sent to it, count it, and look for
                work when it returns.";
            attribute :>> criterion = "In 100 rounds of work handed to every online
                processor, each processor runs each round once and every secondary
                takes 1 or more inter-processor interrupts (the `smp` line).";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("smp::on_ipi", "smp::secondary_main");
        }
        requirement <'L.smp.12'> aNewcomerIsCovered : ItemLowLevel {
            attribute :>> statement = "secondary_main shall mark its processor online
                before it reads the shootdown generation, and flush its TLB after, so
                that a shootdown racing its arrival either waits for it or is
                covered by that flush; and shall answer every grace period requested
                before it came online.";
            attribute :>> criterion = "A secondary held between reading the
                generation and flushing while a shootdown is taken reads, once let
                go, the translation the shootdown changed; a grace period requested
                before it came online completes without its interrupt.";
            attribute :>> parent = "H.MEM.12";
            attribute :>> unit = "smp::secondary_main";
        }
        requirement <'L.smp.13'> theSchedulerTakesEveryProcessor : ItemLowLevel {
            attribute :>> statement = "Once the scheduler has started, secondary_main
                shall hand its processor to the scheduler's idle loop and not return
                to waiting for work.";
            attribute :>> criterion = "After sched::init, tasks pinned one to each
                online processor all run, each on its own processor: N of N.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = "smp::secondary_main";
        }
    }

    package Shootdown {
        doc /* Dropping stale translations from every processor's TLB: the
             * whole-TLB shootdown, the turn that keeps shootdowns one at a
             * time, the wait for every processor's answer and its bound. The
             * Arm pair invalidates by broadcast in hardware, so the waits here
             * are x86-64's. */

        requirement <'L.smp.14'> aShootdownReachesEveryProcessor : ItemLowLevel {
            attribute :>> statement = "When flush_tlb_everywhere returns, no online
                processor shall still translate through an entry it held before it
                was called.";
            attribute :>> criterion = "Over 20 rounds in which every online processor
                reads a kernel page and the page is then moved to another frame,
                every processor reads the page's new value each round: 0 stale reads
                (the `tlb` line), on each architecture.";
            attribute :>> parent = "H.MEM.12";
            attribute :>> unit = ("smp::flush_tlb_everywhere", "smp::service_tlb",
                "smp::on_ipi", "smp::wait_for");
        }
        requirement <'L.smp.15'> oneShootdownAtATime : ItemLowLevel {
            attribute :>> statement = "take_turn shall let one shootdown hold the
                turn at a time, and a processor waiting for it shall answer the
                shootdowns of others while it waits.";
            attribute :>> criterion = "Two processors each requesting a shootdown
                while a third holds the turn both complete once it is let go, one
                after the other: 0 generations taken by two holders at once, and 0
                waits for the turn ending in FX-0001.";
            attribute :>> parent = "H.MEM.12";
            attribute :>> unit = ("smp::take_turn", "smp::flush_tlb_everywhere");
        }
        requirement <'L.smp.16'> aWaiterAnswersWhereItIs : ItemLowLevel {
            attribute :>> statement = "Where invalidation is not broadcast, a task
                waiting for the shootdown turn shall answer shootdowns for the
                processor it is running on at each moment, never for one it has
                left.";
            attribute :>> criterion = "On x86-64 with 3 or more processors online, a
                task waiting for the turn that moves to another processor answers a
                generation requested with no interrupt on the processor it moved to,
                and the processor it left is not recorded as answering (the
                `migrate` line).";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::as_this_cpu", "smp::take_turn",
                "smp::service_tlb");
        }
        requirement <'L.smp.17'> aProcessorThatNeverAnswersIsFatal : ItemLowLevel {
            attribute :>> statement = "wait_for shall stop the kernel in the safe
                state, naming the processor, once its stuck bound -- 10 s of
                wall-clock time and patience's count of polls for it -- has passed
                without an answer, and not before both.";
            attribute :>> criterion = "A processor made to stop answering its
                interrupt ends the boot in FX-0001 naming it, after 10 s or more and
                167,772,160 polls or more, under tcg and under KVM.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("smp::wait_for", "smp::patience");
        }
        requirement <'L.smp.18'> aStoppedHolderIsFatal : ItemLowLevel {
            attribute :>> statement = "take_turn shall stop the kernel in the safe
                state when the shootdown generation has stood still for 40 s and
                patience's count of polls while it waits for the turn, and shall
                start its count again whenever the generation moves.";
            attribute :>> criterion = "A holder made to stop with the turn in hand
                ends the boot in FX-0003 after 40 s or more; a holder that keeps
                taking generations for longer than that is waited for.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("smp::take_turn", "smp::patience");
        }
        requirement <'L.smp.19'> theInterruptIsSentAgain : ItemLowLevel {
            attribute :>> statement = "wait_for shall send its interrupt again every
                10 ms while a processor it waits for has not answered.";
            attribute :>> criterion = "With a processor's first interrupt dropped, the
                shootdown still completes, the processor answering the second.";
            attribute :>> parent = "H.MEM.12";
            attribute :>> unit = "smp::wait_for";
        }
        requirement <'L.smp.20'> theShootdownRulesAreAsserted : ItemLowLevel {
            attribute :>> statement = "A shootdown shall be refused with a
                diagnostic when it is requested holding a lock that disables
                preemption, or, once tasks run, when it would wait for another
                processor with interrupts masked.";
            attribute :>> criterion = "A shootdown requested holding a SpinLock, and
                one that would wait for another processor with interrupts masked,
                each stop the kernel with the assertion naming the rule, in a build
                with debug assertions.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("smp::shootdown_requested",
                "smp::shootdown_waits_for_others", "smp::flush_tlb_everywhere",
                "smp::invalidate_pages");
        }
        requirement <'L.smp.21'> farBehindFlushesEverything : ItemLowLevel {
            attribute :>> statement = "service_tlb shall flush the whole TLB of a
                processor more than one generation behind, or whose generation's
                request is not a scoped one or was rewritten while it was read, and
                shall record the latest generation answered without taking back a
                later one.";
            attribute :>> criterion = "A processor two generations behind, one a
                generation behind a whole-TLB request, and one reading a scoped
                request whose generation changes under the read each flush the
                whole TLB; tlb_seen after each is the latest generation, never less
                than a value recorded meanwhile.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::service_tlb", "smp::scoped_request");
        }
    }

    package ScopedShootdown {
        doc /* The shootdown for a change to one address space: the pages that
             * changed, on the processors that may hold that space's
             * translations, and the page tables it gives back afterwards
             * (finding F-36). The mask an address space keeps of those
             * processors is here too. */

        requirement <'L.smp.22'> aScopedRequestIsReadByItsSet : ItemLowLevel {
            attribute :>> statement = "scoped_request shall give a processor in a
                scoped request's set that request's pages, or the whole TLB where it
                asks for more than the ceiling, and a processor outside the set
                nothing to invalidate.";
            attribute :>> criterion = "For a request of 3 pages to processors 1 and
                3, processor 1 is given the 3 pages and processor 2 none; for a
                request of the whole TLB, a member is given everything.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::scoped_request", "smp::service_tlb");
        }
        requirement <'L.smp.23'> aScopedShootdownReachesItsSet : ItemLowLevel {
            attribute :>> statement = "flush_tlb_pages shall invalidate the pages it
                names on the online processors in its set other than the one it runs
                on before it returns.";
            attribute :>> criterion = "On a machine with two processors or more, a
                child that wrote a shared page and holds still on its processor has
                the page protected read-only from this one, whose scoped shootdown
                names the child's processor: the child's next write ends it with
                SIGSEGV, 11, instead of answering (the `rmap` line), on each
                architecture.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::flush_tlb_pages", "smp::invalidate_pages",
                "smp::service_tlb", "smp::scoped_request", "smp::wait_for");
        }
        requirement <'L.smp.24'> aScopedShootdownGivesBackItsTables : ItemLowLevel {
            attribute :>> statement = "flush_tlb_pages shall give back every page
                table its pages hold once its invalidation has returned, and none
                before it is called.";
            attribute :>> criterion = "Of the tables an unmap_in emptied, 0 are given
                back before flush_tlb_pages is called and every one is given back by
                it, its list left empty, with an empty set of processors.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = "smp::flush_tlb_pages";
        }
        requirement <'L.smp.25'> nobodyToAskIsAskedNothing : ItemLowLevel {
            attribute :>> statement = "invalidate_pages shall interrupt no processor
                when there is nothing to invalidate, when its set is empty, before
                the processors are discovered, when this processor is the only one
                online or where invalidation is broadcast, and shall invalidate the
                pages on this processor itself in the last three.";
            attribute :>> criterion = "Each of those five calls raises the unsent
                count by 1 or leaves it (nothing to invalidate) and sends 0
                interrupts; after the last three a translation of the named page
                cached before is gone on this processor.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::invalidate_pages", "smp::flush_here",
                "smp::TlbPages::is_empty");
        }
        requirement <'L.smp.26'> pageSetsStopAtTheCeiling : ItemLowLevel {
            attribute :>> statement = "A TlbPages shall list each page asked for
                once, up to 33 pages, ask for the whole TLB past that, and when
                merged with another shall list both sets' pages once each and ask for
                the whole TLB if either did.";
            attribute :>> criterion = "33 pages added by range and one again by an
                address inside it list 33 pages; 34 by range ask for everything; two
                sets of 1 and 2 pages sharing one merge to 2 pages in order; a merge
                with a set asking for everything asks for everything.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::TlbPages::add", "smp::TlbPages::add_range",
                "smp::TlbPages::add_all", "smp::TlbPages::addresses",
                "smp::TlbPages::is_everything");
        }
        requirement <'L.smp.27'> aMaskKeepsWhoJoined : ItemLowLevel {
            attribute :>> statement = "A CpuMask's snapshot shall hold every
                processor that joined it and has not left since.";
            attribute :>> criterion = "A mask joined by processors 0 and 3 and left by
                0 snapshots as 3 alone.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("smp::CpuMask::join", "smp::CpuMask::leave",
                "smp::CpuMask::snapshot");
        }
        requirement <'L.smp.28'> setsAreJoined : ItemLowLevel {
            attribute :>> statement = "add_cpus shall add every processor of one set
                to another and remove none.";
            attribute :>> criterion = "{0, 2} added to {1, 2} gives {0, 1, 2}; the
                empty set added changes nothing.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = "smp::add_cpus";
        }
    }

    package GracePeriods {
        doc /* Read-side sections and the wait for every one of them to end:
             * what lets a writer free an object it has unpublished. Nothing in
             * the product calls synchronize yet (src/kernel/src/irq.rs names the
             * handler unregistration it is for); stage 4's grace check does. */

        requirement <'L.smp.29'> aGracePeriodOutlastsEveryReader : ItemLowLevel {
            attribute :>> statement = "synchronize shall return only once every
                online processor has been outside a read-side section since it was
                called.";
            attribute :>> criterion = "A writer that replaces an object every other
                online processor is reading in read_section, synchronizes and
                poisons the replaced one, 100 times over, leaves 0 reads of a
                poisoned object among more than 0 reads (the `grace` line).";
            attribute :>> parent = "H.MEM.19";
            attribute :>> unit = ("smp::synchronize", "smp::read_section",
                "smp::answer_grace", "smp::on_ipi", "smp::wait_for");
        }
    }

    package Stopping {
        doc /* What a panic asks of the other processors, and where they look. */

        requirement <'L.smp.30'> aPanicAsksTheOthersToStop : ItemLowLevel {
            attribute :>> statement = "stop_others shall set the stop request and
                interrupt every other online processor, and return how many it
                asked: none before the processors are discovered.";
            attribute :>> criterion = "A panic on one of N online processors reports
                N-1 asked (the `stopped` line), and 0 before discovery.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "smp::stop_others";
        }
        requirement <'L.smp.31'> aProcessorStopsWhereItLooks : ItemLowLevel {
            attribute :>> statement = "Once a panic has asked every processor to
                stop, a processor shall halt on its next inter-processor interrupt,
                and on its next poll while it waits for the shootdown turn or for
                other processors' answers.";
            attribute :>> criterion = "With a panic on one processor while the others
                are idle, waiting for the turn and waiting for answers, each other
                processor halts: 0 lines printed and 0 shootdowns completed by them
                after the report begins.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("smp::halt_if_stopping", "smp::on_ipi",
                "smp::take_turn", "smp::wait_for");
        }
    }

    package Kick {
        doc /* The interrupt the scheduler sends a processor it has given work. */

        requirement <'L.smp.32'> aKickReachesItsTarget : ItemLowLevel {
            attribute :>> statement = "interrupt_one shall interrupt the online
                processor it names, and every other processor where the architecture
                cannot address it alone, where that processor is not online, or
                before the processors are known.";
            attribute :>> criterion = "On x86-64 a kick to processor 2 raises its
                inter-processor interrupt count by 1 and the others' by 0; a kick to
                a processor that is not online raises every other online
                processor's.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = "smp::interrupt_one";
        }
        requirement <'L.smp.33'> aLateAnswerIsWaitedFor : ItemLowLevel {
            attribute :>> statement = "wait_for shall keep waiting for a processor
                that has not answered once its late bound and patience's count for
                it have passed, and shall count the answer as late when it comes.";
            attribute :>> criterion = "A processor kept from answering, inside a
                read-side section, past a wait's late bound is waited for: a grace
                period on every architecture and a shootdown on x86-64 each return
                once it answers, with the processor among those counted late, and
                the boot goes on (the `late` line).";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("smp::wait_for", "smp::flush_everywhere_within",
                "smp::synchronize_within");
        }
        requirement <'L.smp.34'> aLateAnswerIsReportedAfterTheTurn : ItemLowLevel {
            attribute :>> statement = "The late answers of a shootdown or a grace
                period shall be counted, with the longest wait, and reported with
                the time waited only once the wait has returned and the shootdown
                turn is given back: a line for each of the first four, then one each
                time the count reaches a power of two.";
            attribute :>> criterion = "With a processor starved by the host past
                the late bound, the boot log shows a line for each of the first four
                late answers and then one at 8, 16, 32 and so on, each printed with
                the turn free; no line is printed from inside a wait.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "smp::report_late";
        }
    }
}
