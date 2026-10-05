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
                a tenth of a second.";
            attribute :>> criterion = "With the port's transmit interrupt suppressed, a
                line queued reaches the port at the first write made after 100 ms,
                and the stall count rises by 1.";
            attribute :>> parent = ("H.BOOT.8", "H.TRAP.14");
            attribute :>> unit = ("console::output::Transmit::unstall",
                "console::output::stalls");
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
                or not, in the recent-output ring and on the screen console, and a
                program's output in neither.";
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
                the command line holds ferrix.fbcon and there is a framebuffer: clear
                it, and draw what recent keeps of the lines printed before, from the
                first whole one.";
            attribute :>> criterion = "A boot without ferrix.fbcon leaves the
                framebuffer as firmware left it; one with it shows the boot's earlier
                lines from a line's start, and every later kernel line.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::screen::start", "console::screen::put");
        }
        requirement <'L.console.37'> theBoardFitsTheScreen : ItemLowLevel {
            attribute :>> statement = "Board::new shall pick the largest glyph scale up
                to 4 that leaves at least 60 columns, inside the margins and the top
                inset, and refuse a surface with room for no column or fewer than 2
                rows.";
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
                that finds the board taken shall not be drawn.";
            attribute :>> criterion = "After stop, 0 bytes are drawn; stop called by a
                processor part way through a draw returns after at most 1,000,000
                attempts at the board.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("console::screen::stop", "console::screen::put");
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
}
