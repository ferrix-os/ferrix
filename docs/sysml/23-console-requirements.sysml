/*
 * Ferrix — SysML v2 model, part 23: the low-level requirements of console.
 *
 * What each unit of src/kernel/src/console.rs and src/kernel/src/console/ does, as
 * `ItemLowLevel` requirements (part 13 defines the format, part 14 is the
 * pilot this copies): the kernel's lines to the port, whole and in order;
 * the way a failure report gets past a lock nobody will release; the
 * recent-output ring a panic screen draws; the transmit ring and the writers
 * who queue into it, wait for room or poll; the receive ring the port's
 * interrupt fills; the kernel log every byte is recorded in, what it
 * promises a reader and what it keeps out; and the boot console drawn on the
 * framebuffer. The ports themselves are each architecture's
 * (src/kernel/src/arch/<isa>/console.rs) and belong to the arch slices; the two
 * of their functions that decide *which* console the kernel writes to, and
 * whose checks test that, are here too (`Ports`), at the coordinator's asking.
 *
 * The pilot's rules hold: a requirement names every function that carries
 * its behaviour as its `unit`; one check proves a whole requirement, so a
 * requirement is no broader than one check can prove; a statement says no
 * more than its criterion tests; an accessor needs no requirement of its own;
 * check code living in a product file is listed in
 * tools/common/data/traceability-units.json, not required -- here
 * `console::input::check` and `console::output::check`, which read their
 * rings' private state and so cannot move to a check file without becoming
 * child modules. Until they do, the requirements they prove (`L.console.9`,
 * `.10` and `.20`) wait in the baseline.
 *
 * What proves what. console/log_check.rs (FX-0505, the `log` line) drives
 * rings of its own through wrap, overrun, a partial read and two racing
 * writers, and then requires of the kernel log that a task's write and a
 * line written as a failure report are in it and a line sent unlogged is
 * not. service_check.rs reads back the recent-output ring. xtask's
 * `test-init-file` ends a boot in the FX-1501 panic and requires its line on
 * the port. arch/aarch64/check.rs reads and refuses `console=ramoops,...`
 * values, and arch/armv7a/check.rs holds the console a device tree chooses
 * to the port it must be. Nothing tests a report written while the port's
 * lock is held for good, the transmit ring full, a stalled port or the
 * screen console; docs/certification/IMPLEMENTATION.md W-8 lists them.
 *
 * Ids are `L.console.<n>`, numbered through the subsystem; the package gives
 * the part of it. They are never renumbered once a check names them.
 */
package FerrixConsoleRequirements {
    private import FerrixItemRequirements::*;

    package Lines {
        doc /* The kernel's own lines, from println! to the port. */

        requirement <'L.console.1'> nothingBeforeThePort : ItemLowLevel {
            attribute :>> statement = "write_as, emit and drain shall send nothing to
                the port, and record nothing in the kernel log, the recent-output ring
                or the screen, until mark_ready has said the port is configured.";
            attribute :>> criterion = "A line and a program's write made before
                mark_ready leave the port untouched and the kernel log's and the
                recent-output ring's counts where they were; the same made after it
                raise both.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::write_as", "console::emit", "console::drain",
                "console::mark_ready");
        }
        requirement <'L.console.2'> aLineIsWholeOnThePort : ItemLowLevel {
            attribute :>> statement = "write_as shall put a formatted line into the
                transmit ring or the port under one hold of the port's lock, each bare
                newline as CRLF, so that no other writer's bytes fall inside it.";
            attribute :>> criterion = "1,000 lines of 200 bytes printed from every
                processor at once reach the port each whole and ending in CRLF: 0
                lines with another writer's bytes inside.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::write", "console::write_as",
                "console::Writer::new", "console::Writer::bytes",
                "console::Writer::write_str", "console::Writer::finish");
        }
        requirement <'L.console.3'> aPollerGoesAfterTheRing : ItemLowLevel {
            attribute :>> statement = "A writer that may not queue -- the port's
                interrupt not installed, its processor's interrupts masked, or a
                failure report begun -- shall first poll out everything the transmit
                ring holds, then write its own bytes straight to the port.";
            attribute :>> criterion = "With bytes queued in the transmit ring, a line
                printed with interrupts masked reaches the port after every one of
                them, and the ring is empty when the print returns.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::may_queue", "console::Writer::new",
                "console::Writer::put", "console::output::Transmit::flush");
        }
    }

    package ProgramOutput {
        doc /* A program's bytes, through write_bytes and write_raw, and the
             * transmit ring a writer queues them into. */

        requirement <'L.console.4'> aProgramsBytesAsWritten : ItemLowLevel {
            attribute :>> statement = "emit shall send a program's bytes to the port
                in the order given, each bare newline as CRLF for write_bytes and
                every byte as it is for write_raw.";
            attribute :>> criterion = "A write_bytes of text holding bare newlines,
                and a write_raw of the same, reach the port with CRLF for each newline
                in the first and LF alone in the second, every other byte the same
                and in order.";
            attribute :>> parent = "H.TRAP.14";
            attribute :>> unit = ("console::write_bytes", "console::write_raw",
                "console::emit", "console::Writer::bytes");
        }
        requirement <'L.console.5'> aWriterThatMayNotWaitDoesNot : ItemLowLevel {
            attribute :>> statement = "emit shall send a program's bytes through
                write_waiting only from a task that may sleep, and otherwise queue
                them without waiting, Transmit::queue polling the oldest queued byte
                out to the port to make room when the ring is full.";
            attribute :>> criterion = "A program's write made with a spin lock held
                returns without sleeping and its line reaches the port; with the
                ring full, each byte it queues sends one queued byte first and
                counts it as polled.";
            attribute :>> parent = "H.SCHED.11";
            attribute :>> unit = ("console::emit", "console::output::Transmit::queue");
        }
        requirement <'L.console.6'> aTasksWriteWaitsForRoom : ItemLowLevel {
            attribute :>> statement = "write_waiting shall queue a task's write only
                when the ring has room for the whole of the next chunk, and otherwise
                sleep until the ring drains to half or a tenth of a second passes,
                never polling out bytes other writers queued.";
            attribute :>> criterion = "A task writing 16 KiB to a port that sends
                slower than it writes sleeps at least once, the ring never holds more
                than 4,096 bytes, 0 bytes are polled out by it, and every byte
                reaches the port in order.";
            attribute :>> parent = "H.SCHED.11";
            attribute :>> unit = ("console::output::write_waiting",
                "console::output::try_queue", "console::output::Transmit::room");
        }
        requirement <'L.console.7'> chunksEndAtANewline : ItemLowLevel {
            attribute :>> statement = "next_chunk shall take the whole of what is left
                when it costs at most 2,048 bytes on the wire, a newline sent as CRLF
                costing two; otherwise the longest part within that cost that ends in
                a newline, or failing one the longest part within it.";
            attribute :>> criterion = "What is left of 100 bytes is taken whole; of
                3,000 bytes with newlines at the 1,000th and 2,500th, the first 1,000;
                of 3,000 bytes with none, 2,048; with CRLF asked for, each newline
                counted as 2 in the cost.";
            attribute :>> parent = "H.SCHED.10";
            attribute :>> unit = "console::output::next_chunk";
        }
        requirement <'L.console.8'> aWriteIsLoggedOnce : ItemLowLevel {
            attribute :>> statement = "try_queue shall record a chunk in the kernel
                log once, when it is queued, and never for a try that found no room.";
            attribute :>> criterion = "A task's write that finds the ring without room
                twice before it is queued appears in the kernel log exactly once.";
            attribute :>> parent = "H.TRAP.15";
            attribute :>> unit = "console::output::try_queue";
        }
        requirement <'L.console.9'> thePortTakesABurst : ItemLowLevel {
            attribute :>> statement = "Transmit::pump shall give the port no more than
                it reports room for and no more than 64 bytes, never waiting for room,
                and charge them to the count of whoever pumped.";
            attribute :>> criterion = "A task's line of 172 bytes on the wire, written
                on a port with an interrupt, has all but at most 64 of them sent by
                the transmit interrupt (the `output` line).";
            attribute :>> parent = "H.SCHED.10";
            attribute :>> unit = ("console::output::Transmit::pump",
                "console::output::by_writer");
        }
        requirement <'L.console.10'> theInterruptSendsTheRest : ItemLowLevel {
            attribute :>> statement = "The port's interrupt shall give the port what it
                has room for of the transmit ring, and settle shall keep the port's
                transmit interrupt enabled exactly while the ring holds bytes, so
                that what a writer left is sent with no further write.";
            attribute :>> criterion = "A task's write left in the ring is sent within 2
                seconds with no further write and 0 stalls polled out (the `output`
                line).";
            attribute :>> parent = ("H.BOOT.8", "H.TRAP.14");
            attribute :>> unit = ("console::output::on_interrupt",
                "console::output::Transmit::settle", "console::output::start",
                "console::input::on_interrupt");
        }
        requirement <'L.console.11'> writersWakeAtHalf : ItemLowLevel {
            attribute :>> statement = "settle shall ask for waiting writers to be woken
                when the ring drains from above half its length to half or less, and
                a writer shall wake them only after it has dropped the port's lock.";
            attribute :>> criterion = "A task waiting for room in a full ring is woken
                once the ring holds 2,048 bytes or fewer, and not while it holds
                more.";
            attribute :>> parent = "H.SCHED.11";
            attribute :>> unit = ("console::output::Transmit::settle",
                "console::output::wake_writers", "console::Writer::finish");
        }
        requirement <'L.console.12'> aStalledPortIsPolledOut : ItemLowLevel {
            attribute :>> statement = "unstall shall poll the transmit ring out to the
                port, and count a stall, when the port has taken none of its bytes for
                a tenth of a second; and when the transmit check (FX-0504) finds a
                task's write still in the ring after the port's transmit interrupt
                has sent nothing for 50 ms, or still there two seconds later, the
                failures it does not stop for, fall_back_to_polling shall stop
                queuing and poll out what the ring holds, so that writers poll as
                before the port's interrupt was installed, with an output WARNING
                line. Every other failure of the check stops the boot.";
            attribute :>> criterion = "With the port's transmit interrupt suppressed, a
                line queued reaches the port at the first write made after 100 ms,
                and the stall count rises by 1. With the transmit interrupt never
                raised on a reference boot, the boot prints the WARNING line naming
                50 ms, given up after less than 100 ms, and test-boot fails on it;
                with the writer made not to queue, the boot stops with FX-0504.";
            attribute :>> parent = ("H.BOOT.8", "H.TRAP.14");
            attribute :>> unit = ("console::output::Transmit::unstall",
                "console::output::stalls", "console::output::fall_back_to_polling",
                "console::output::Failure::fmt", "console::output::Failure::falls_back");
        }
        requirement <'L.console.13'> drainSendsEverything : ItemLowLevel {
            attribute :>> statement = "drain shall poll out everything the transmit
                ring holds and then wait for the port to have sent it before it
                returns.";
            attribute :>> criterion = "A line printed just before drain and a power
                off reaches the port whole, on each architecture, 3 of 3.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "console::drain";
        }
    }

    package FailureReports {
        doc /* Writing once begin_panic has been called, and the recent-output
             * ring a panic screen and the screen console draw. */

        requirement <'L.console.14'> aReportReachesThePort : ItemLowLevel {
            attribute :>> statement = "Once begin_panic has been called, write_as
                shall write through write_panicking, which, when it gets the port's
                lock, sends the line straight to the port.";
            attribute :>> criterion = "A boot told to panic at its end
                (`ferrix.onexit=panic`) shows the FX-1501 line on the port after
                init's exit (`cargo xtask test-init-file`).";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::begin_panic", "console::write_as",
                "console::write_panicking");
        }
        requirement <'L.console.15'> aReportPassesAHeldLock : ItemLowLevel {
            attribute :>> statement = "write_panicking shall try the port's lock at
                most 10,000,000 times and then send the line straight to the port
                without it, recorded in the kernel log if asked, leaving what was
                queued in the ring.";
            attribute :>> criterion = "A report written while another context holds
                the port's lock for good reaches the port whole after at most
                10,000,000 attempts, and is in the kernel log.";
            attribute :>> parent = "H.FAIL.4";
            attribute :>> unit = ("console::write_panicking",
                "console::Unlocked::write_str");
        }
        requirement <'L.console.16'> drainInAReportIsBounded : ItemLowLevel {
            attribute :>> statement = "During a failure report, drain shall try the
                port's lock at most 10,000,000 times, leave the ring as it is if it
                never gets it, and wait for the port either way.";
            attribute :>> criterion = "drain called during a report while another
                context holds the port's lock for good returns after at most
                10,000,000 attempts, having waited for the port to empty.";
            attribute :>> parent = "H.FAIL.4";
            attribute :>> unit = "console::drain";
        }
        requirement <'L.console.17'> theLastLinesAreKept : ItemLowLevel {
            attribute :>> statement = "recent shall give back, oldest byte first, the
                most recent bytes of the kernel's own lines as they were printed.";
            attribute :>> criterion = "A line printed is found as printed, its newline
                without the CR, in the last 1,024 bytes recent copies out.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::remember", "console::recent",
                "console::Writer::bytes", "console::Unlocked::write_str");
        }
        requirement <'L.console.18'> onlyKernelLinesAreKept : ItemLowLevel {
            attribute :>> statement = "Writer::bytes shall keep a kernel line, logged
                or not, in the recent-output ring, and a program's output not, and
                give both to the screen console.";
            attribute :>> criterion = "After a program's write of a marker line and a
                kernel line, the last 4,096 bytes recent gives hold the kernel line
                and not the marker.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::Writer::bytes", "console::Kind::remembered");
        }
        requirement <'L.console.19'> theRecentRingIsBounded : ItemLowLevel {
            attribute :>> statement = "recent shall copy at most its buffer's length
                and at most the 4,096 bytes the ring keeps, and a writer racing it
                shall leave a byte stale at worst.";
            attribute :>> criterion = "recent into a 16-byte buffer copies 16 bytes;
                into an 8,192-byte buffer after 10,000 bytes printed, 4,096, the last
                printed; with another processor printing throughout, every copy
                stays within those bounds.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::recent", "console::remember");
        }
    }

    package Input {
        doc /* What the port receives, between its interrupt and the reader. */

        requirement <'L.console.20'> theInputRingKeepsOrder : ItemLowLevel {
            attribute :>> statement = "receive shall move what the port gives into the
                receive ring in order, dropping and counting every byte that finds it
                full, except the one byte an interrupt remapping check armed, which it
                takes once and keeps out of the ring, and pop and has_input shall give the reader those bytes in
                order and say without a lock whether any are held.";
            attribute :>> criterion = "4,099 bytes offered to the empty ring come back
                as the first 4,096 in order, a 4,097th pop finds nothing, has_input
                says so before and after, and the overrun count rises by exactly 3
                (the `input` line).";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::input::receive", "console::input::pop",
                "console::input::Ring::push", "console::input::Ring::pop",
                "console::input::has_input", "console::input::overruns");
        }
        requirement <'L.console.21'> aReaderIsWoken : ItemLowLevel {
            attribute :>> statement = "receive shall wake the input's waiters, after
                it has dropped the ring's lock, whenever it added a byte.";
            attribute :>> criterion = "A task waiting for input with the ring empty is
                woken by the first byte the port receives, within the interrupt that
                took it.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::input::receive", "console::input::waiters");
        }
        requirement <'L.console.22'> theHandlerIsBounded : ItemLowLevel {
            attribute :>> statement = "receive shall take at most 8,192 bytes from the
                port per call, and on_interrupt shall make at most 68 passes of
                receiving and transmitting, stopping at the first the port says
                nothing is pending after.";
            attribute :>> criterion = "A port that never reports itself empty lets
                the handler return after 68 passes of at most 8,192 received bytes
                each; a port with nothing pending after the first pass, after 1.";
            attribute :>> parent = "H.SCHED.10";
            attribute :>> unit = ("console::input::on_interrupt",
                "console::input::receive");
        }
        requirement <'L.console.23'> theInterruptIsInstalledFirst : ItemLowLevel {
            attribute :>> statement = "init shall register the port's receive
                interrupt and mark input and output interrupt-driven before it enables
                the line; answer None, installing nothing, when the machine gives the
                port no interrupt this kernel can take; and refuse when the interrupt
                is already held.";
            attribute :>> criterion = "On QEMU's x86-64, AArch64 and ARMv7-A machines
                the `input` line says the port receives by an interrupt; a second init
                of the same number is refused with nothing changed; a machine whose
                port has no interrupt leaves input polled.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::input::init", "console::output::start");
        }
        requirement <'L.console.24'> aReadTakesFromTheRing : ItemLowLevel {
            attribute :>> statement = "read_byte shall take from the receive ring once
                the receive interrupt is installed, and poll the port only until
                then.";
            attribute :>> criterion = "Before init a byte comes through the poll; after
                it, a byte received by interrupt comes through the ring and the poll
                is called 0 times.";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("console::input::read_byte",
                "console::input::interrupt_driven");
        }
    }

    package Log {
        doc /* The kernel log: what it records, what it promises a reader, and
             * what it keeps out. */

        requirement <'L.console.25'> theCountIsExact : ItemLowLevel {
            attribute :>> statement = "Ring::record shall claim exactly one place per
                byte, so that the ring's count of bytes recorded is exact however
                many writers race.";
            attribute :>> criterion = "Two writers recording 20,000 bytes each at
                once, on two processors where there are two, leave the ring's count
                at exactly 40,000.";
            attribute :>> parent = "H.TRAP.15";
            attribute :>> unit = ("console::log::Ring::record",
                "console::log::Ring::record_byte", "console::log::Ring::slot");
        }
        requirement <'L.console.26'> aWrappedRingKeepsItsLast : ItemLowLevel {
            attribute :>> statement = "A ring of N bytes written past its length shall
                keep its last N - 1 bytes in order, and unread shall count exactly
                those for a reader at its start.";
            attribute :>> criterion = "A 64-byte ring given 100 bytes answers unread
                63 for a cursor at 0, and its reads from there give the last 63 bytes
                in order.";
            attribute :>> parent = "H.TRAP.13";
            attribute :>> unit = ("console::log::Ring::read",
                "console::log::Ring::copy_from", "console::log::Ring::slot",
                "console::log::Ring::unread", "console::log::Ring::oldest_at");
        }
        requirement <'L.console.27'> aLateReaderIsToldWhatItLost : ItemLowLevel {
            attribute :>> statement = "read shall move a cursor behind the oldest byte
                kept up to it and report exactly the bytes it skipped as lost.";
            attribute :>> criterion = "A reader at 0 of a 64-byte ring given 100 bytes
                is told 37 lost; caught up and then 200 bytes behind, it is told 137
                lost and given the 63 kept.";
            attribute :>> parent = "H.TRAP.13";
            attribute :>> unit = ("console::log::Ring::read",
                "console::log::Ring::oldest_at");
        }
        requirement <'L.console.28'> aPartialReadResumes : ItemLowLevel {
            attribute :>> statement = "read shall copy no more than the reader's buffer
                holds and move the cursor past exactly what it copied, so that the
                next read goes on from there.";
            attribute :>> criterion = "A 10-byte read with 63 unread leaves 53 unread,
                and the next read gives those 53 in order with 0 lost and 0 left
                unread.";
            attribute :>> parent = "H.TRAP.13";
            attribute :>> unit = ("console::log::Ring::read", "console::log::Ring::unread");
        }
        requirement <'L.console.29'> racingWritersNeverMix : ItemLowLevel {
            attribute :>> statement = "read shall hand a reader only bytes writers
                recorded, dropping and counting as lost any a writer replaced while
                they were being copied, so that every byte recorded is either read or
                counted lost.";
            attribute :>> criterion = "While two writers record 20,000 bytes each into
                a 256-byte ring, a reader reading 97 bytes at a time reads only bytes
                they wrote, and the bytes it read and was told it lost come to exactly
                40,000.";
            attribute :>> parent = "H.TRAP.13";
            attribute :>> unit = ("console::log::Ring::read",
                "console::log::Ring::copy_from", "console::log::Ring::record_byte");
        }
        requirement <'L.console.30'> aCursorAheadIsBroughtBack : ItemLowLevel {
            attribute :>> statement = "read shall bring a cursor ahead of the newest
                byte back to it, copying nothing and reporting nothing lost.";
            attribute :>> criterion = "A read at a cursor 1,000 past the ring's count
                copies 0 bytes, reports 0 lost and leaves the cursor at the count.";
            attribute :>> parent = "H.TRAP.13";
            attribute :>> unit = "console::log::Ring::read";
        }
        requirement <'L.console.31'> aProgramsOutputIsLogged : ItemLowLevel {
            attribute :>> statement = "A task's write to the console shall be
                recorded in the kernel log as the task wrote it, without the CR the
                port is sent.";
            attribute :>> criterion = "A task's write of a line is found, byte for byte
                and with no CR, in the last 4,096 bytes of the kernel log.";
            attribute :>> parent = "H.TRAP.15";
            attribute :>> unit = ("console::output::try_queue", "console::Writer::bytes",
                "console::Kind::logged", "console::log::record");
        }
        requirement <'L.console.32'> aReportIsLogged : ItemLowLevel {
            attribute :>> statement = "write_panicking, asked to log, shall record the
                line in the kernel log when it writes it under the port's lock.";
            attribute :>> criterion = "A line written through write_panicking with
                logging asked for, the panicking flag not set, is found in the last
                4,096 bytes of the kernel log.";
            attribute :>> parent = "H.TRAP.15";
            attribute :>> unit = ("console::write_panicking", "console::Writer::bytes");
        }
        requirement <'L.console.33'> kernelLinesAreLogged : ItemLowLevel {
            attribute :>> statement = "write shall record a kernel line in the kernel
                log as printed, before its newline becomes CRLF.";
            attribute :>> criterion = "A line printed with println! is found, without
                a CR, in the last 4,096 bytes of the kernel log.";
            attribute :>> parent = "H.TRAP.15";
            attribute :>> unit = ("console::write", "console::write_as",
                "console::Writer::bytes");
        }
        requirement <'L.console.34'> anUnloggedLineIsKeptOut : ItemLowLevel {
            attribute :>> statement = "write_unlogged shall keep its line out of the
                kernel log.";
            attribute :>> criterion = "A line sent with println_unlogged! is not in the
                last 4,096 bytes of the kernel log, read after it was sent.";
            attribute :>> parent = "H.BOOT.9";
            attribute :>> unit = ("console::write_unlogged", "console::Kind::logged",
                "console::Writer::bytes");
        }
        requirement <'L.console.35'> recordingNeverWaits : ItemLowLevel {
            attribute :>> statement = "record shall take no lock, allocate nothing and
                wake no reader, so that a writer recording in any context, a panic's
                included, is never held up by the log.";
            attribute :>> criterion = "A byte recorded from an interrupt handler, from
                the idle task and from a failure report written while another
                processor holds every lock the kernel has is recorded, with 0 heap
                allocations and 0 wakes.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::log::record", "console::log::Ring::record",
                "console::log::Ring::record_byte");
        }
    }

    package Screen {
        doc /* The boot console drawn on the framebuffer firmware left, which a
             * machine whose port is out of reach -- the Pixel 7 -- shows until a
             * panic draws its own screen over it. */

        requirement <'L.console.36'> theScreenStartsWhenAsked : ItemLowLevel {
            attribute :>> statement = "screen::start shall draw the console only when
                the command line holds ferrix.fbcon and there is a framebuffer, and
                only the first time it is called: clear it, and draw what recent
                keeps of the lines printed before, from the first whole one; and
                milestone shall paint stripe n, for n below 12, only while the
                console is drawn, and not when it finds the board taken.";
            attribute :>> criterion = "A boot without ferrix.fbcon leaves the
                framebuffer as firmware left it; one with it shows the boot's earlier
                lines from a line's start, and every later kernel line.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::screen::start", "console::screen::put",
                "console::screen::milestone", "console::screen::Board::stripe");
        }
        requirement <'L.console.37'> theBoardFitsTheScreen : ItemLowLevel {
            attribute :>> statement = "Board::new shall pick the largest glyph scale up
                to 4, or up to 1 under ferrix.fbcon.small, that leaves at least 60
                columns, inside the margins, the top inset and the 12 milestone
                stripes, and refuse a surface with room for no column or fewer than
                2 rows.";
            attribute :>> criterion = "A 1080-pixel-wide surface gets scale 2 and 65
                columns; a 640-pixel one, scale 1; a surface 20 pixels high is
                refused.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "console::screen::Board::new";
        }
        requirement <'L.console.38'> glyphsGoWhereTheCursorIs : ItemLowLevel {
            attribute :>> statement = "Board::put shall ignore a CR, start a new row at
                a newline, advance a tab to the next stop of eight, draw a printable
                byte and draw anything else as the replacement character, wrapping at
                the last column and from the bottom row to the top, and keep the row
                after the newest line blank.";
            attribute :>> criterion = "Drawn on a board of 3 rows, a line with a tab, a
                CR and a byte 0x01 puts its glyphs at the columns the rule gives, the
                replacement glyph for 0x01, and after a fourth newline the text is on
                row 1 with row 2 blank.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::screen::Board::put",
                "console::screen::Board::glyph", "console::screen::Board::newline",
                "console::screen::Board::clear_row");
        }
        requirement <'L.console.39'> theScreenStopsForGood : ItemLowLevel {
            attribute :>> statement = "screen::stop shall stop the screen console for
                good and wait a bounded time for a draw in flight to finish, and a byte
                that finds the board taken shall not be drawn; and every draw shall
                make its stores globally visible before the board is released.";
            attribute :>> criterion = "After stop, 0 bytes are drawn; stop called by a
                processor part way through a draw returns after at most 1,000,000
                attempts at the board. The drain is build evidence and an argument:
                every draw of start, put and milestone ends with
                arch::drain_write_combining (SFENCE on x86-64, nothing on the Arm
                kernels) under the board, on every boot's path; no emulator models
                write-combining fill buffers, so no control can show it fired, and
                the photographed hardware evidence is owed by a docs/BACKLOG.md row.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::screen::stop", "console::screen::put",
                "console::screen::start", "console::screen::milestone",
                "arch::x86_64::framebuffer::drain_write_combining");
        }
    }

    package Ports {
        doc /* Which console the kernel writes to, where firmware or the loader
             * says. The ports' drivers are the arch slices'. */

        requirement <'L.console.40'> aRamoopsZoneIsReadOrRefused : ItemLowLevel {
            attribute :>> statement = "ramoops_zone shall read the loader's
                `console=ramoops,<base>,<size>` as a zone only when both numbers are
                hexadecimal with a 0x prefix, no third field follows, the base is a
                page's and the size is more than the record's 12-byte header.";
            attribute :>> criterion = "`ramoops,0x9ff00000,0x40000` is read as the zone
                at 0x9ff00000 of 0x40000 bytes; a base not a page's, a 12-byte zone, a
                missing size, a third field, a number without 0x and `ttyAMA0` are
                each refused: 0 of 6 taken (the AArch64 `machine` line).";
            attribute :>> parent = "H.BOOT.1";
            attribute :>> unit = "arch::aarch64::console::ramoops_zone";
        }
        requirement <'L.console.41'> theTreeChoosesThePort : ItemLowLevel {
            attribute :>> statement = "chosen shall take the port a `console=` in the
                device tree's bootargs names, where the tree has that port with
                registers; else the port stdout-path names; else the first port the
                kernel drives that the tree has not turned off.";
            attribute :>> criterion = "Of 4 trees built for the check, `console=pl011`
                against a stdout-path naming the USART gives the PL011; a
                `console=ttyS0` falls through to stdout-path's PL011; no stdout-path
                gives the first port not disabled; a lone STM32 USART gives it: 4 of
                4 (the ARMv7-A `machine` line).";
            attribute :>> parent = "H.BOOT.8";
            attribute :>> unit = ("arch::armv7a::console::chosen",
                "arch::armv7a::console::forced", "arch::armv7a::console::first_enabled",
                "arch::armv7a::console::Port::of", "arch::armv7a::console::Port::named");
        }
    }

    package Pstore {
        doc /* src/kernel/src/console/pstore.rs: the kernel log kept across a reset
             * in a region of RAM the loader gives, which the next boot's loader
             * reports (docs/POSTMORTEM.md section 3; the consultant's OK IF Q1-Q16,
             * ledger line 1238). The region's format is ferrix-bootinfo's
             * pstore module, unclassified, host-tested; the decisions below are
             * the kernel's. */

        requirement <'L.console.42'> theRegionHoldsWhatTheLogRecords : ItemLowLevel {
            attribute :>> statement = "The pstore mirror shall hold in the region's
                ring what the kernel log records, from the boot's first logged byte,
                in order, with nothing the log does not record: the lines sent
                unlogged are in neither.";
            attribute :>> criterion = "In test-postmortem's boot 1, the region saved
                over QMP after the report validates as boot 1's, and its text, line
                for line, is boot 1's serial log from the kernel's first line with
                only the unlogged lines missing, and holds none of them (x86-64 and
                AArch64).";
            attribute :>> parent = "H.TRAP.15";
            attribute :>> unit = ("console::pstore::record", "console::pstore::adopt",
                "console::log::record_into", "console::log::slot_of", "console::log::record");
        }
        requirement <'L.console.43'> attachingFailsClosed : ItemLowLevel {
            attribute :>> statement = "attach shall mirror the log into the loader's
                region only when it is whole pages, a header page and a ring of a
                power of two from 4 KiB to 16 MiB, wholly inside one Pstore entry of
                the memory map, RAM the direct map covers at both ends, and its header
                is valid with nothing recorded; else it shall say why in one line and
                write nothing there.";
            attribute :>> criterion = "At boot, placement refuses a region made up
                inside the kernel image's entry and, around the loader's region, one
                not on a page, one no longer than its header, one whose ring is not a
                power of two and one running past its entry, and takes the loader's
                region: 5 of 5 refused when a region was attached, 1 of 1 when none.
                The header clause is ferrix-bootinfo's validate, whose host tests
                refuse a wrong magic, version, header length, capacity, region
                length, CRC and count: every_field_is_checked and
                capacities_outside_the_bounds_are_refused. The kernel takes the
                header through ferrix-bootinfo's fresh, whose host test
                the_kernel_takes_only_a_fresh_version_2_header refuses a version-1
                header, a count other than 0 and every stop word other than 0. By
                inspection of check: nothing is written to the region before every
                clause passes (adopt is its only writer).";
            attribute :>> parent = "H.BOOT.1";
            attribute :>> unit = ("console::pstore::attach", "console::pstore::check",
                "console::pstore::placement");
        }
        requirement <'L.console.44'> theFirstReportWritesTheRegionBack : ItemLowLevel {
            attribute :>> statement = "conclude shall write the pstore region back from
                the caches after the report's last line is drained and before the
                screen is drawn, taking no lock and allocating nothing; an abridged
                report shall write nothing back. finish shall write it back, through
                stop, after its last line and before the reset ferrix.onexit=reset
                asks for or the power-off of its default.";
            attribute :>> criterion = "In test-postmortem's boot 1, the unlogged
                `written back for the next boot` line comes straight after the
                report's explanation, and the region saved after it holds that
                explanation (x86-64 and AArch64); in test-boot --reset, the loader
                that comes back quotes the kernel's resetting line as the log's last
                (x86-64 and AArch64). Neither write-back is observable under QEMU,
                whose caches are the host's: the hardware run is a BACKLOG row.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::pstore::write_back", "panic::conclude",
                "power::finish");
        }
        requirement <'L.console.46'> eachStopOnPurposeRecordsItself : ItemLowLevel {
            doc /* docs/POSTMORTEM.md section 5 (S1); the consultant's OK IF C1-C12,
                 * ledger line 2749. */
            attribute :>> statement = "On a path that does not return, each stop on
                purpose shall record its code in the attached region's stop word, as
                one atomic update after its caller's last line and before the
                architecture's stop: reboot(2)'s RESTART and RESTART2 a restart,
                POWER_OFF a power-off, HALT a halt, finish a reset under
                ferrix.onexit=reset and a power-off by default; begin_report shall
                record a panic when it admits the first report, and an abridged
                report nothing. No stop shall replace a panic's word.";
            attribute :>> criterion = "In test-postmortem's stop cases (x86-64 and
                AArch64), boot 2's loader says boot 1 ended in a restart after
                reboot -f and after a RESTART2 word, a power-off after poweroff -f, a
                halt after halt -f, a reset after init exited under
                ferrix.onexit=reset, and a power-off after init exited by default;
                no stop recorded after a reset from outside while a program sleeps;
                and in its record case, the saved region's word and boot 2's loader
                both say a panic. By inspection: an abridged report stores nothing,
                and stop's update leaves a panic's word as it is.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::pstore::stop", "console::pstore::panicked",
                "panic::begin_report", "power::finish");
        }
        requirement <'L.console.47'> rebootWritesTheRegionBack : ItemLowLevel {
            doc /* reboot(2) itself, syscall/system.rs's sys_reboot, is load-ring code
                 * and cannot be a unit here: its four calls are argued by
                 * inspection, and the unit is what they call. */
            attribute :>> statement = "reboot(2) shall write the pstore region back
                from the caches, through stop, in each of its four stopping arms
                (RESTART, RESTART2, POWER_OFF, HALT), after its last line and before
                the architecture's reset or power-off, taking no lock and allocating
                nothing.";
            attribute :>> criterion = "In test-postmortem's stop cases (x86-64 and
                AArch64), boot 2's loader reads the word each of the RESTART,
                RESTART2, POWER_OFF and HALT arms' stop call stored. By inspection:
                stop writes the region back in the same body after the store, and
                nothing in each arm can return after it. The write-back itself is
                not observable under QEMU, whose caches are the host's: the N150
                run is a BACKLOG row.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = "console::pstore::stop";
        }
        requirement <'L.console.48'> theStopWordChecksItself : ItemLowLevel {
            doc /* The word's format is ferrix-bootinfo's pstore module, which the
                 * loader and the kernel compile and which is unclassified (the
                 * consultant's L3, ledger line 2749): its units are named here by
                 * the kernel functions that store it. */
            attribute :>> statement = "The stop word the kernel stores shall be
                ferrix-bootinfo's Stop::word: a stop's code in its low half and the
                code's complement in its high half, so that a word of 0 reads as no
                stop recorded, a word whose halves do not match or whose code is 0
                as damaged, and a whole word with a code the list does not name as
                unknown; validate shall read version 1, with no stop word whatever
                its bytes hold, and version 2, and refuse any other.";
            attribute :>> criterion = "Host tests of ferrix-bootinfo's pstore:
                every_stop_round_trips_and_none_is_zero,
                a_changed_bit_is_damaged (every single-bit change of every stop's
                word), an_unassigned_code_is_a_newer_kernels, and
                version_1_reads_with_no_stop_word_and_version_3_is_refused. By
                inspection: stop and panicked store Stop::word and nothing else.";
            attribute :>> parent = "H.BOOT.1";
            attribute :>> unit = ("console::pstore::stop", "console::pstore::panicked");
        }
    }

    package Lockup {
        doc /* The lockup detector (src/kernel/src/lockup.rs, item) and the watch
             * it runs on (src/kernel/src/watch.rs, core): docs/POSTMORTEM.md
             * section 6.2; the consultant's OK IF D1-D14, ledger line 2889. */

        requirement <'L.lockup.1'> theOptionTurnsItOnOnlyOnOn : ItemLowLevel {
            attribute :>> statement = "read_option shall ask for the detector for
                exactly the value on of ferrix.lockup; no option or off shall leave
                it off, and any other value shall be said as not understood and
                leave it off.";
            attribute :>> criterion = "In test-postmortem, the lockup case's boot
                with ferrix.lockup=on says the detector watches; every boot without
                the option says nothing of it and is never reported as a lockup. By
                inspection: a value other than on and off is said and leaves it
                off.";
            attribute :>> parent = "H.FAIL.6";
            attribute :>> unit = ("lockup::read_option", "lockup::asked", "lockup::start");
        }
        requirement <'L.lockup.2'> everyProcessorTicksAndBeats : ItemLowLevel {
            attribute :>> statement = "Once the watch has started, every online
                processor shall count each timer and inter-processor interrupt it
                takes in its heartbeat and keep a one-shot armed no later than one
                second ahead, from its tick and before it halts idle; until the watch
                starts, each of these shall be one read of a flag set once.";
            attribute :>> criterion = "In test-postmortem's quiet case (x86-64
                under KVM with 4 processors, AArch64) the detector watches while every
                processor idles for 25 s and reports nothing; in the lockup case the
                check sees every online processor's heartbeat move before it masks
                one. The watch tick re-arms at every expiry (watch::tick), and the idle
                path arms it again before every halt: by inspection each alone keeps a
                processor ticking once its first watch tick is armed. Controls: the
                heartbeat's bump removed makes the quiet case report FX-9007; the
                watch tick's arming removed, both arms at once (arm_here arms
                nothing), leaves the detector dead. By inspection: with the watch not
                started, tick, beat and arm_here return after reading WATCHING.";
            attribute :>> parent = "H.FAIL.6";
            attribute :>> unit = ("watch::start", "watch::tick", "watch::beat",
                "watch::arm_here", "watch::beats", "watch::register");
        }
        requirement <'L.lockup.3'> aStuckProcessorIsReported : ItemLowLevel {
            attribute :>> statement = "On its tick, each watcher shall read the
                heartbeat of the next online processor in logical order; one that
                stood still for 10 seconds shall have the stop word set to a lockup
                and, on x86-64, be named and sent an NMI whose handler, first of
                all, reports from its own frame, the watcher reporting itself when
                no report has begun 2 seconds later, decided on a later tick; on the
                Arm architectures the watcher shall report at once, naming it.
                Nothing shall be judged while a panic stops the processors.";
            attribute :>> criterion = "In test-postmortem's lockup case, the report
                carries the lockup's code 9.5 to 30 s after the check masks the boot
                processor: on x86-64 under KVM the masked processor's own, the
                instruction its NMI interrupted in the check's spin; on AArch64 the watcher's, naming it.
                Controls: the heartbeat's bump removed reports where nothing is
                stuck; the ring check removed leaves the masking unreported; on
                x86-64 the NMI's arm removed leaves the watcher's own report; on
                AArch64 the ring check removed leaves the masking unreported. By
                inspection: nothing is judged while smp::stopping().";
            attribute :>> parent = "H.FAIL.6";
            attribute :>> unit = ("lockup::on_tick", "lockup::stuck", "lockup::send_nmi",
                "lockup::target_of", "watch::name_for_nmi", "watch::named_here",
                "arch::x86_64::send_lockup_nmi");
        }
        requirement <'L.console.49'> aLockupsWordIsKept : ItemLowLevel {
            attribute :>> statement = "locked_up shall store a lockup's stop word,
                code 7; panicked shall not replace it, and stop shall replace
                neither a lockup's word nor a panic's.";
            attribute :>> criterion = "In test-postmortem's lockup case the next
                loader says the boot ended in a lockup, though a panic's report
                followed the store; by inspection, stop's update keeps both words.";
            attribute :>> parent = "H.FAIL.6";
            attribute :>> unit = ("console::pstore::locked_up", "console::pstore::panicked",
                "console::pstore::stop", "console::pstore::failed");
        }
    }

    package PanicReset {
        doc /* `ferrix.panic=reset` (src/kernel/src/panic/reset.rs), the N150
             * stop-gap of 2026-10-17 (consultant ledger line 2143): the reset a
             * failure report ends in when asked. Its own family, L.panic, since
             * panic.rs had no low-level ids; here beside the pstore it exists
             * for. The device-tree read of the option gets an id when it is
             * built. */

        requirement <'L.panic.1'> theOptionArmsOnlyOnReset : ItemLowLevel {
            attribute :>> statement = "read_option shall arm the reset once, for
                exactly the value reset of ferrix.panic; no option or halt shall
                leave it unarmed, and any other value shall be said as not
                understood and leave it unarmed.";
            attribute :>> criterion = "In test-postmortem, a boot with
                ferrix.panic=reset says it resets after the report; one with
                ferrix.panic=Reset says the value is not understood, its report
                says it halts and holds no reset line; one with no option holds no
                reset line.";
            attribute :>> parent = "H.FAIL.5";
            attribute :>> unit = ("panic::reset::read_option", "panic::reset::armed");
        }
        requirement <'L.panic.2'> theReportAnnouncesTheReset : ItemLowLevel {
            attribute :>> statement = "When armed, conclude shall say in its stopped
                line that the processor resets the machine, print and drain the reset
                line before the pstore write-back and the screen, and after the
                screen wait and reset instead of halting.";
            attribute :>> criterion = "In test-postmortem's reset case boot 1's
                report says the processor resets the machine after the report, holds
                the reset line, and the machine starts again by itself.";
            attribute :>> parent = "H.FAIL.5";
            attribute :>> unit = ("panic::conclude", "panic::reset::announce");
        }
        requirement <'L.panic.3'> theWaitIsTheCounterOnly : ItemLowLevel {
            attribute :>> statement = "wait_then_reset shall wait 10 seconds on the
                free-running counter, reading nothing but counter_hz and counter_now,
                taking no lock and allocating nothing, wait not at all when the rate
                is 0, and then call arch::reset.";
            attribute :>> criterion = "In test-postmortem's reset case the loader's
                banner comes again at least 9.5 s and at most 60 s after the reset
                line. By inspection: the function's body is the counter loop and
                the reset.";
            attribute :>> parent = "H.FAIL.5";
            attribute :>> unit = "panic::reset::wait_then_reset";
        }
        requirement <'L.panic.4'> aFaultInTheScreenStillResets : ItemLowLevel {
            attribute :>> statement = "When armed and the reset announced, abridged
                on the processor that announced it shall go on to wait_then_reset;
                on any other processor it shall halt. (On x86-64 a fault taken before
                the kernel's GS base is installed reads another register value and
                may halt: the fail-safe direction.)";
            attribute :>> criterion = "A fault forced inside screen::draw under
                ferrix.panic=reset still ends in the reset in test-postmortem's
                reset case, and fails it with abridged's reset removed (gate.sh
                control, the commit names both runs). By inspection: resetting_here
                compares this processor's per-CPU register with the announcer's.";
            attribute :>> parent = "H.FAIL.5";
            attribute :>> unit = ("panic::abridged", "panic::reset::resetting_here");
        }
    }
}
