/*
 * Ferrix — SysML v2 model, part 13: the certified item's requirements.
 *
 * The high-level requirements of the certified item (the `core` and `item`
 * rings of docs/certification/ITEM.md): what each subsystem promises at its
 * interface, decomposed from the Security Target's objectives
 * (docs/certification/SECURITY-TARGET.md §4.1 and §8.2) and the safety
 * manual's assumed safety requirements (SAFETY-MANUAL.md §2). The low-level
 * requirements, one per unit of code, go in one file per subsystem after this
 * one. docs/certification/IMPLEMENTATION.md W-8 is the design, and
 * tools/common/check/check-traceability.py the gate that reads these and writes
 * docs/certification/TRACEABILITY.md.
 */
package FerrixItemRequirements {
    private import ScalarValues::*;
    private import FerrixStructure::Kernel;

    requirement def ItemRequirement {
        doc /* A requirement of the certified item. Unlike the goal and the
             * design rules of part 1, which say why the system is shaped as it
             * is, each of these says what the item shall do in terms a test
             * can pass or fail, and names the requirement above it that it
             * serves. The gate refuses one without all three. */
        subject item : Kernel;
        attribute statement : String {
            doc /* One sentence with "shall", about behaviour a program, a
                 * device or the boot log can observe. */
        }
        attribute criterion : String {
            doc /* The pass/fail condition a check tests, counted where there
                 * is something to count. */
        }
        attribute parent : String[1..*] {
            doc /* The ids of the level above: for a high-level requirement a
                 * Security Target objective (O.*), an assumed safety
                 * requirement (ASR-*) or a goal requirement (G.*); for a
                 * low-level one, a high-level requirement (H.*). */
        }
    }

    requirement def ItemHighLevel :> ItemRequirement {
        doc /* What one subsystem of the item promises at its interface, with
             * an id `H.<AREA>.<n>`. The areas follow the families of
             * functional requirement the item implements. */
    }

    requirement def ItemLowLevel :> ItemRequirement {
        doc /* What one unit of the item's code does, with an id
             * `L.<module>.<n>`, and the unit named so that a renamed function
             * breaks the trace loudly. */
        attribute unit : String[1..*] {
            doc /* `path::function` or `path::Type::method`, relative to
                 * src/kernel/src: `mm::zero_frame`; or, for a library crate
                 * of the item, the same from the crate's src/ led by the
                 * crate's name: `ferrix_btrfs::volume::Volume::read_node`. */
        }
    }

    // -----------------------------------------------------------------------
    // The high level, one package per area. The areas follow the families of
    // SECURITY-TARGET.md §8.2 the item implements, and the safety manual's
    // ASR-1 to ASR-8; a requirement names every objective it serves.
    // -----------------------------------------------------------------------

    package Memory {
        doc /* Address spaces, the frames under them and the mappings that
             * reach them: O.ISOLATE, O.WXN and O.SCRUB, ASR-1, ASR-2 and
             * ASR-5 (FDP_IFC.1, FDP_IFF.1, FDP_RIP.2). */

        requirement <'H.MEM.1'> spacesAreSeparate : ItemHighLevel {
            attribute :>> statement = "A user address space shall translate a user
                virtual address only to a frame its own mappings name, so that one
                address in two address spaces reaches two frames unless a VMO both
                map shares one.";
            attribute :>> criterion = "Of N reads of one user address, made
                alternately in two address spaces that each wrote a different value
                there, all N return the reading space's own value and none the
                other's (the boot's `spaces` line, N = 128).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.2'> kernelMemoryIsNotUsers : ItemHighLevel {
            attribute :>> statement = "No mapping of kernel memory shall be
                accessible from user mode: a user-mode read, write or instruction
                fetch at a kernel address shall end in a fault delivered to the
                thread, never in the access.";
            attribute :>> criterion = "A program that reads, writes and jumps to a
                kernel-half address is ended with SIGSEGV each time, 3 of 3, on every
                architecture, and the kernel's memory at that address is unchanged.";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.3'> kernelKeptOutOfUserPages : ItemHighLevel {
            attribute :>> statement = "On every processor the kernel shall run with
                its own access to user pages refused outside the window a user copy
                opens: SMEP and SMAP on x86-64, PAN on AArch64 where the processor
                implements it. ARMv7-A has no such control (AoU-6).";
            attribute :>> criterion = "The boot reports SMEP on and SMAP on (x86-64),
                or PAN on where offered (AArch64), for each of the N online
                processors; a kernel read of a user page outside the window faults.";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.4'> writableOrExecutable : ItemHighLevel {
            attribute :>> statement = "Once bring-up has dropped the loader's
                identity map, no leaf mapping under the kernel's own root table or
                the identity root shall be both writable and executable.";
            attribute :>> criterion = "The boot's sweep of every leaf mapping under
                the kernel's root and the identity root counts N mappings, E > 0 of
                them executable, and 0 both writable and executable (the `w^x`
                line).";
            attribute :>> parent = ("O.WXN", "ASR-2");
        }
        requirement <'H.MEM.5'> imageIsSealed : ItemHighLevel {
            attribute :>> statement = "No mapping of the frames that hold the
                kernel's text and read-only data, the direct map's alias of them
                included, shall be writable.";
            attribute :>> criterion = "The boot's sweep of every mapping of those
                frames finds 0 writable, and finds the direct map aliasing every byte
                of them (the `sealed` line).";
            attribute :>> parent = ("O.WXN", "ASR-2");
        }
        requirement <'H.MEM.6'> framesZeroedBeforeUse : ItemHighLevel {
            attribute :>> statement = "A physical frame shall hold only zeros when it
                is committed to a VMO, mapped into a user address space for the first
                time, or made a page table.";
            attribute :>> criterion = "A frame written with a pattern, freed and
                committed again reads 0 non-zero bytes of its 4096 before the new
                owner's first write, for a VMO page and for a page table alike.";
            attribute :>> parent = ("O.SCRUB", "ASR-5");
        }
        requirement <'H.MEM.7'> nothingFreedBeforeItsShootdown : ItemHighLevel {
            attribute :>> statement = "A page-table frame an unmap empties shall not
                return to the frame allocator until the TLB invalidation that covers
                the unmap has been made.";
            attribute :>> criterion = "A page table an unmap empties is still held,
                not freed, until the shootdown covering it completes: 0 tables freed
                before it, and exactly the tables it emptied given back by it.";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.8'> privateCopiesStayPrivate : ItemHighLevel {
            attribute :>> statement = "After a fork, a write by the parent or the
                child to a private page shall not be visible to the other.";
            attribute :>> criterion = "The `cow` program, in which each side writes a
                page both shared, exits 61 (neither saw the other's write).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.9'> framesComeBack : ItemHighLevel {
            attribute :>> statement = "Every frame an address space, a VMO or a
                process holds shall return to the frame allocator when the last
                reference to it goes.";
            attribute :>> criterion = "After each memory and program check, the free
                frame count is what it was before: 0 frames leaked, per check (the
                `objects`, `uaccess` and `exits` lines).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.MEM.10'> imageRangeRefused : ItemHighLevel {
            attribute :>> statement = "Every interface that maps a physical range a
                caller names shall refuse a range that touches the kernel image.";
            attribute :>> criterion = "Requests naming the image's first page, its
                last page and a range straddling its end are each refused, 0
                accepted, at stages 1, 2 and 6.";
            attribute :>> parent = ("O.ISOLATE", "O.WXN");
        }
        requirement <'H.MEM.11'> allocationFailureIsAnswered : ItemHighLevel {
            attribute :>> statement = "An allocation the item makes after bring-up
                shall report failure to its caller, which shall answer it as running
                out of memory and keep nothing half made.";
            attribute :>> criterion = "With every allocation of the swept memory
                operations and native calls failed in turn, each failure is absorbed
                or answered NO_MEMORY or ENOMEM, and 0 frames or objects are left
                behind (the `sweep` and `no-mem` lines).";
            attribute :>> parent = "O.QUOTA";
        }
        // Split out of H.MEM.7 and H.MEM.5 by W-8 step 4 (mm and user), so that
        // one check proves each.
        requirement <'H.MEM.12'> remapsAreSeenEverywhere : ItemHighLevel {
            attribute :>> statement = "Once a change to a kernel mapping returns,
                every online processor shall translate the address as changed, and
                none through the translation it replaced.";
            attribute :>> criterion = "Of N moves of one page to another frame, each
                made after every online processor has read the page, every
                processor reads the moved page's new value each time: 0 stale reads
                (the `tlb` line).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.13'> theTextCannotBeWritten : ItemHighLevel {
            attribute :>> statement = "A write to the kernel's text through its
                direct-map alias shall fault, on every architecture.";
            attribute :>> criterion = "A kernel write to the first word of the text
                through the direct map ends in the page-fault report (FX-9001) on
                x86-64, AArch64 and ARMv7-A, 3 of 3, and the word is unchanged.";
            attribute :>> parent = ("O.WXN", "ASR-2");
        }
        // Added by W-8 step 4 (mm and user): behaviour of the kernel's own memory
        // and of user mappings that no requirement above said, and the halves of
        // H.MEM.7 and H.MEM.8 that one check each proves.
        requirement <'H.MEM.14'> kernelAllocationsHaveOneOwner : ItemHighLevel {
            attribute :>> statement = "The kernel's own frame allocator, heap and
                address arena shall hand each frame, block and range to one holder
                at a time, and shall never hand out frame 0.";
            attribute :>> criterion = "Stage 2's memory check finds two frames
                distinct, a 16-frame block 16-frame aligned, frame 0 neither managed
                nor handed out, a Box, a 4096-entry Vec and a 2048-key BTreeMap each
                holding what was put in it, and two arena ranges distinct, inside
                the arena and two guard pages apart.";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.15'> kernelMappingsAreWhatWasAsked : ItemHighLevel {
            attribute :>> statement = "A mapping the kernel makes in its own address
                space shall translate each page to the frame or register asked for,
                with the permissions and memory type asked for.";
            attribute :>> criterion = "Each page of an 8-page arena range translates
                to a frame, none to its neighbour's; a page protected read-only reads back not
                writable in the descriptors and still translates; a device window at
                offset 0x40 translates to the register asked for, offset included,
                as device memory and not executable, and is gone once unmapped.";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.16'> sharedWritesAreSeen : ItemHighLevel {
            attribute :>> statement = "After a fork, a write by the child to a
                MAP_SHARED mapping shall be visible to the parent, and its write to a
                MAP_PRIVATE mapping shall not.";
            attribute :>> criterion = "The `shared` program exits 62 (its MAP_SHARED
                write reached the parent and its MAP_PRIVATE write did not).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.17'> framesWaitForTheirInvalidation : ItemHighLevel {
            attribute :>> statement = "A frame taken out of a translation, of a user
                address space or of the kernel's, shall not return to the frame
                allocator until the TLB invalidation that covers it has been made on
                every processor that may hold the translation.";
            attribute :>> criterion = "A frame an unmap, a decommit or a replace
                takes out of a translation that another processor has cached is
                still allocated until that processor's shootdown returns: 0 frames
                freed before it, on every architecture.";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.MEM.18'> userMappingsKeepTheirPermissions : ItemHighLevel {
            attribute :>> statement = "The kernel shall refuse to map a device
                window or a native VMO executable into a user address space, and
                shall map the vDSO's data page read-only.";
            attribute :>> criterion = "An executable device window and an executable
                vmo_map are each refused, 0 mapped; the vDSO's data page reads back
                not writable, and a write to it faults.";
            attribute :>> parent = ("O.WXN", "O.ISOLATE");
        }
        // Added by W-8 step 4 (smp): what a grace period promises, which no
        // requirement above said.
        requirement <'H.MEM.19'> gracePeriodsOutlastReaders : ItemHighLevel {
            attribute :>> statement = "Once a grace period the kernel waits for has
                ended, no read-side section that was running on any processor when
                the wait began shall still be running.";
            attribute :>> criterion = "A writer that replaces an object every other
                online processor is reading, waits a grace period and poisons the
                replaced one, 100 times over, leaves 0 reads of a poisoned object
                among more than 0 reads (the `grace` line).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
    }

    package Objects {
        doc /* Handles, rights and the objects they name: O.CAPABILITY and
             * ASR-3 (FDP_ACC.1, FDP_ACF.1, FMT_MSA.1, FMT_MSA.3). */

        requirement <'H.OBJ.1'> onlyThroughAHandle : ItemHighLevel {
            attribute :>> statement = "A native call shall act on an object only
                through a handle in the calling process's own handle table, and shall
                refuse a value naming no live handle there with BAD_HANDLE.";
            attribute :>> criterion = "Every native call given a value never issued,
                a value from another process's table and a value closed earlier is
                refused BAD_HANDLE: 0 of them act on an object.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.2'> rightsAreChecked : ItemHighLevel {
            attribute :>> statement = "A native call shall be refused with
                WRONG_TYPE when its handle names an object of another type, and with
                ACCESS_DENIED unless its handle carries every right the operation
                requires.";
            attribute :>> criterion = "For each native operation that requires a
                right, a call through a handle lacking it is refused ACCESS_DENIED,
                and a call through a handle of the wrong type WRONG_TYPE; 0 are
                performed.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.3'> duplicatesNeverRaise : ItemHighLevel {
            attribute :>> statement = "Duplicating a handle shall need the DUPLICATE
                right and yield rights that are a subset of the source's; asking for
                a right the source lacks shall be refused with ACCESS_DENIED.";
            attribute :>> criterion = "A duplicate of a handle without DUPLICATE is
                refused ACCESS_DENIED; one asking for a right the source lacks is
                refused ACCESS_DENIED and the source keeps its rights; a narrower
                copy is made with the rights asked for.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.4'> staleValuesRefused : ItemHighLevel {
            attribute :>> statement = "A handle value shall stop naming its object
                when the handle is closed or transferred, and shall not name the
                object its slot holds next.";
            attribute :>> criterion = "A value whose handle was closed is refused
                BAD_HANDLE, on a get and on a second close, after its slot is
                reused, and the reused slot issues a new value.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.5'> handlesMoveOnlyInMessages : ItemHighLevel {
            attribute :>> statement = "A handle shall pass from one process to another
                only inside a channel message, leaving the sender's table in the same
                step, and a write that is refused shall leave every handle it carried
                with the sender.";
            attribute :>> criterion = "After a write carrying handles each is refused
                BAD_HANDLE in the sender and works in the reader; writes refused
                PEER_CLOSED, TOO_BIG and SHOULD_WAIT leave every carried handle
                usable by the sender; a handle without TRANSFER is refused
                ACCESS_DENIED.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.6'> newProcessHoldsNothing : ItemHighLevel {
            attribute :>> statement = "A process shall start holding no handle but
                the bootstrap handle its creator explicitly passes, and a fork shall
                not copy the native handle table.";
            attribute :>> criterion = "A created process holds 0 handles before it
                starts and exactly 1 after a start that passed a bootstrap; a forked
                child's native handle table is empty.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.7'> tableIsBounded : ItemHighLevel {
            attribute :>> statement = "A process's handle table shall hold at most
                4096 handles, refuse an insertion past that with NO_HANDLES, and lose
                nothing it could not deliver.";
            attribute :>> criterion = "Duplicating until refused ends in NO_HANDLES
                with at most 4096 held; a channel read and a VMO create into the full
                table are refused NO_HANDLES and the message stays queued (the
                `refusals` line).";
            attribute :>> parent = ("O.CAPABILITY", "O.QUOTA");
        }
        requirement <'H.OBJ.8'> objectsEndWithTheirLastHandle : ItemHighLevel {
            attribute :>> statement = "An object shall be destroyed when the last
                handle to it closes.";
            attribute :>> criterion = "One object of each kind, made and its last
                handle closed, is destroyed by the time the close returns or the
                drain after it ends: 0 of them outlive it, and 0 frames are
                leaked.";
            attribute :>> parent = ("O.CAPABILITY", "O.QUOTA");
        }
        requirement <'H.OBJ.9'> killingAJobEndsItsTree : ItemHighLevel {
            attribute :>> statement = "Killing a job shall end every process in it
                and in every job beneath it.";
            attribute :>> criterion = "4 processes in a tree of three jobs are all
                ended by two kills, and each wait on them is woken (the `jobs`
                line).";
            attribute :>> parent = ("O.CAPABILITY", "O.QUOTA");
        }

        // H.OBJ.10 to H.OBJ.17 were added by the low-level pilot on object/
        // (IMPLEMENTATION.md W-8, step 3). H.OBJ.10, 12 and 13 are behaviour
        // object/ implements that no requirement said; H.OBJ.14 to 17 are the
        // halves split out of H.OBJ.3, 4, 8 and 11, so that one check can prove
        // each whole, since a `Verifies:` tag credits the whole requirement.
        // H.OBJ.8 itself is now one check of every kind of object, which does
        // not exist yet; its low-level requirements prove it kind by kind.

        requirement <'H.OBJ.10'> queuesAreBounded : ItemHighLevel {
            attribute :>> statement = "Every queue a program can fill in the kernel
                shall be bounded -- a channel end's unread messages at 256, a port's
                user packets at 1024, an object's port registrations at 64 -- and an
                addition past the bound shall be refused without being queued.";
            attribute :>> criterion = "Writing to a channel nobody reads is refused
                SHOULD_WAIT at its bound, and queueing on a port nobody takes from is
                refused SHOULD_WAIT at exactly 1024 packets, 0 queued past either
                bound, and draining the channel makes room again; the 65th
                registration on one object is refused.";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.OBJ.11'> waitsEndAtTheirSignal : ItemHighLevel {
            attribute :>> statement = "A wait on an object shall end when a signal
                it waits for is asserted, at once if one already is, and shall not
                end before its deadline while none is.";
            attribute :>> criterion = "A wait on an empty channel end with a zero
                deadline times out; one with a two-minute deadline is ended by a
                message written 20 ms later, not before it, and reports READABLE; a
                wait for PEER_CLOSED on an end whose peer has closed returns at
                once.";
            attribute :>> parent = ("G.7", "O.CAPABILITY");
        }
        requirement <'H.OBJ.12'> jobsAreCgroups : ItemHighLevel {
            attribute :>> statement = "The job tree shall be the cgroup v2
                hierarchy: every process in exactly one job, a job populated exactly
                while it or a job beneath it has a member not yet released, and a
                job refusing a process or a child that its killed or removed state,
                its limits or the no-internal-process rule forbid.";
            attribute :>> criterion = "A new process is in the root job and a fork's
                child in its parent's; a job empties at its last member's release
                and wakes its waiters; a move a job refuses leaves the process where
                it was; a removed job takes neither a process nor a child; a child
                past cgroup.max.depth or cgroup.max.descendants is refused.";
            attribute :>> parent = ("G.8", "O.QUOTA");
        }
        requirement <'H.OBJ.13'> pidsNameOneProcess : ItemHighLevel {
            attribute :>> statement = "A pid shall name at most one process at a
                time, and a pid given back shall not be handed out again before
                allocation has come round the whole range past it.";
            attribute :>> criterion = "Allocating through the whole range wraps to
                300 and passes over a pid still held: 0 pids handed out twice and
                none below 300 after the wrap.";
            attribute :>> parent = ("G.4", "O.ISOLATE");
        }
        requirement <'H.OBJ.14'> replacesNeverRaise : ItemHighLevel {
            attribute :>> statement = "Replacing a handle shall close the source and
                yield rights that are a subset of its; asking for a right the source
                lacks shall be refused with ACCESS_DENIED and leave the source as it
                was.";
            attribute :>> criterion = "A replace asking for a right not held is
                refused ACCESS_DENIED and the source keeps its rights; a narrower
                replace closes the source and leaves one handle, with the rights
                asked for.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.15'> usedUpSlotsRetire : ItemHighLevel {
            attribute :>> statement = "A slot whose generations are all used shall
                be retired, so that no handle value is ever issued twice.";
            attribute :>> criterion = "After every generation of one slot has been
                issued and closed, the next handle comes from another slot, and 0 of
                the values issued before it are issued again or resolve.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.OBJ.16'> thePeerSeesTheClose : ItemHighLevel {
            attribute :>> statement = "The peer of a destroyed channel end shall
                observe PEER_CLOSED from the moment the close returns, and what the
                destroyed end held shall be freed.";
            attribute :>> criterion = "After one end of a channel holding a queued
                VMO is closed, a read and a write on the survivor are refused
                PEER_CLOSED, 2 of 2, and 0 frames are leaked.";
            attribute :>> parent = ("O.CAPABILITY", "O.QUOTA");
        }
        requirement <'H.OBJ.17'> registrationsFireOnce : ItemHighLevel {
            attribute :>> statement = "A port registration shall queue exactly one
                packet, with its key and the asserted signals it asked for, when one
                of them is first asserted, at once if one already is.";
            attribute :>> criterion = "A registration does not fire before its
                signal, fires once on the message that asserts it and not on a
                second, fires at once for a state already true, and fires when the
                peer closes for PEER_CLOSED, each packet with its key.";
            attribute :>> parent = ("G.7", "O.CAPABILITY");
        }
        requirement <'H.OBJ.18'> theFastPathIsTheGeneralPath : ItemHighLevel {
            attribute :>> statement = "Every result of channel_write_read answered by
                the fast path shall be a result the general path gives in the same
                circumstances, and the fast path shall bypass no filter, audit record
                or speculation barrier. x86-64 only, behind ferrix.fastpath.";
            attribute :>> criterion = "ipc-equiv's transcripts with the fast path on
                and off are the same line for line, with trips taken on and no
                counter moved off (`cargo xtask test-ipc-equiv`); stage 9's fast path
                cases hold both boots to the general path's results (the `fastcase`
                line); a filtered call is refused with the filter's errno.";
            attribute :>> parent = ("G.7", "O.CAPABILITY");
        }
    }

    package Scheduling {
        doc /* Processor time: the fair class, placement and sleep. ASR-8
             * (partially met: the item has no admission control for real-time
             * work, AoU-4, so none is required here) and O.QUOTA's share of a
             * processor (FRU_RSA.1's refinement). */

        requirement <'H.SCHED.1'> runnableTasksRun : ItemHighLevel {
            attribute :>> statement = "Every runnable task shall run: no task shall
                stay runnable without being scheduled while processors are online.";
            attribute :>> criterion = "1000 threads spawned across the online
                processors all run to completion within the check's deadline (the
                `tasks` line).";
            attribute :>> parent = ("G.1", "ASR-8");
        }
        requirement <'H.SCHED.2'> lagIsBounded : ItemHighLevel {
            attribute :>> statement = "Among processor-bound tasks of equal weight on
                one processor, no task's service shall lag its fair share by more
                than one scheduling slice plus the timer overruns served in the
                window.";
            attribute :>> criterion = "With 12 spinners on every processor, each
                processor's worst lag is at or below that bound (the `fair` line).";
            attribute :>> parent = ("ASR-8", "O.QUOTA");
        }
        requirement <'H.SCHED.3'> jobsShareByWeight : ItemHighLevel {
            attribute :>> statement = "Under contention a job shall receive processor
                time in proportion to its weight, whatever the number of tasks in
                it.";
            attribute :>> criterion = "One task alone in a job keeps half of a
                processor, within the check's stated tolerance, against eight tasks
                in a sibling job of equal weight (the `quota` line).";
            attribute :>> parent = ("O.QUOTA", "ASR-8");
        }
        requirement <'H.SCHED.4'> affinityIsObeyed : ItemHighLevel {
            attribute :>> statement = "A task shall run only on the processors its
                affinity mask allows.";
            attribute :>> criterion = "Spinners confined to processors 0 and 1, twice
                as many as there are online processors, are seen running on no other
                processor: 0 violations.";
            attribute :>> parent = "G.1";
        }
        requirement <'H.SCHED.5'> sleepIsNotCutShort : ItemHighLevel {
            attribute :>> statement = "A task that sleeps for a duration shall not be
                woken before the duration has passed, and shall be woken within
                twenty times it.";
            attribute :>> criterion = "A 20 ms sleep returns after at least 20 ms and
                at most 400 ms (the `sleep` line).";
            attribute :>> parent = ("G.1", "ASR-8");
        }
        // Added by W-8 step 4 (smp): the per-processor record everything that
        // runs on a processor finds itself by, which no requirement above said.
        requirement <'H.SCHED.6'> eachProcessorFindsItsOwnRecord : ItemHighLevel {
            attribute :>> statement = "Code running on a processor shall find,
                through that processor's per-processor register, the record that
                names that processor and no other.";
            attribute :>> criterion = "In 100 rounds of work run on every online
                processor at once, each processor runs each round once and the record
                its register gives it names its own hardware identifier every time: 0
                misplaced runs (the `smp` line).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.SCHED.7'> threadLocalDescriptorsPerThread : ItemHighLevel {
            attribute :>> statement = "A thread's thread-local segment descriptors
                and data selectors shall be its own whenever it runs in ring 3,
                never another thread's. x86-64 only.";
            attribute :>> criterion = "Two i386 programs pinned to one processor,
                with one descriptor slot and selector but bases 4 bytes apart, each
                read their own word through %gs 200 times across sched_yield: both
                exit 42.";
            attribute :>> parent = ("O.ISOLATE", "G.1");
        }
        requirement <'H.SCHED.8'> userRegisterStatePerTask : ItemHighLevel {
            attribute :>> statement = "The user-mode state a trap does not save --
                the FS and GS bases and the x87 and SSE registers -- shall be the
                running task's own whenever it runs in ring 3, inherited by a fork
                child and reset by an execve, except that the vector registers of
                a task resumed from a blocking native call may be the initial
                state instead (H.SCHED.12).";
            attribute :>> criterion = "Two programs pinned to one processor, each
                with its own FS and GS bases and XMM, MXCSR and x87 control values,
                read back their own across N yields: 0 mismatches; a fork child
                reads its parent's, and an execve'd image the reset values.";
            attribute :>> parent = ("O.ISOLATE", "G.1");
        }
        requirement <'H.SCHED.9'> vdsoClocksAnswerAsTheCall : ItemHighLevel {
            attribute :>> statement = "A clock read through the vDSO shall answer as
                the system call would, between system calls made on either side of
                it.";
            attribute :>> criterion = "A program reading each of the 7 clocks,
                gettimeofday and time through the vDSO finds each answer between
                system calls made on either side: exit 120.";
            attribute :>> parent = "G.5";
        // H.SCHED.10 and H.SCHED.11 were added by W-8 step 4 (console): the
        // processor time console output may take, which no requirement said.
        requirement <'H.SCHED.10'> consoleHoldsAreBounded : ItemHighLevel {
            attribute :>> statement = "Console output shall hold a processor with
                interrupts masked only for bounded work: a writer shall put at most 64
                bytes into the port and queue at most 2,048 bytes of a task's write
                per hold of the port's lock, and the port's interrupt shall make at
                most 68 passes, each taking at most 8,192 received bytes and giving
                the port at most 64.";
            attribute :>> criterion = "Across a task's 16 KiB write on a port with an
                interrupt, 0 holds of the port's lock put more than 64 bytes into the
                port or queue more than 2,048; a port that never reports itself empty
                lets its interrupt handler return after 68 passes.";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.SCHED.11'> consoleWritersSleepForRoom : ItemHighLevel {
            attribute :>> statement = "A task writing to the console that may sleep
                shall sleep while the fixed 4,096-byte transmit ring has no room for
                its next chunk, and a writer that may not sleep shall never sleep on
                the console.";
            attribute :>> criterion = "A task writing 16 KiB faster than the port
                sends sleeps at least once while the ring never holds more than 4,096
                bytes, and every byte reaches the port in order; a program's write
                made with a spin lock held returns without sleeping and its line
                reaches the port.";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.SCHED.12'> blockedNativeCallVectorState : ItemHighLevel {
            attribute :>> statement = "A task resumed after blocking in
                channel_write_read, object_wait_one or port_wait, made with SYSCALL,
                shall hold in its vector registers either its own state or every
                component's initial state with its own MXCSR and x87 control word,
                never a value another program left. x86-64 only; every other call
                and architecture keeps the whole state (H.SCHED.8).";
            attribute :>> criterion = "A program blocked in one of the calls, every
                vector register its pattern, beside a second program filling them
                with another on its processor, reads back neither pattern and its
                own MXCSR and control word after a message, its peer's close and a
                message with a signal pending; blocked in a Linux call, preempted,
                or past the woken call, it reads its own (the `vectors` line).";
            attribute :>> parent = ("O.ISOLATE", "G.1");
        }
        requirement <'H.SCHED.13'> theDirectSwitchIsThePick : ItemHighLevel {
            attribute :>> statement = "A switch channel_write_read's fast path makes
                without the run queue's pick shall leave every scheduling quantity as
                the pick would leave it, and shall be made only when the pick could
                choose nothing else: the task switched to asleep at home on this
                processor, nothing else waiting and no sleeper due.";
            attribute :>> criterion = "RunQueue::hand_over equals enqueue, the
                rescale, remove_curr and pick_next field for field on 200,000 random
                queues (the ferrix-sched host test); a spinner beside the trips keeps
                its share, and an echo pinned to another processor never runs on the
                caller's (the `fastcase` line).";
            attribute :>> parent = ("G.1", "ASR-8");
        }
    }

    package Interrupts {
        doc /* Interrupt lines given to ring-3 drivers as objects: O.CAPABILITY
             * and ASR-3 for who receives one, O.QUOTA for what an unacknowledged
             * or unclaimed line can cost. */

        requirement <'H.IRQ.1'> deliveredToTheHolder : ItemHighLevel {
            attribute :>> statement = "An interrupt on a line bound to an Interrupt
                object shall be delivered only to that object, waking the task
                waiting on it.";
            attribute :>> criterion = "16 of 16 deliveries end their wait by waking
                it (the `wake` line), and no other waiter is woken.";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.IRQ.2'> maskedUntilAcknowledged : ItemHighLevel {
            attribute :>> statement = "A line bound to an Interrupt object shall be
                delivered to its holder once until the holder acknowledges it; a
                line the interrupt controller holds shall stay masked from its
                delivery until the acknowledgement, and an edge-triggered MSI-X
                vector shall reach the kernel's handler at most 65 times between two
                acknowledgements before it is masked until the next; and the line
                shall be masked when the object is destroyed.";
            attribute :>> criterion = "A line held from delivery to acknowledgement
                is delivered once and 0 times more before the acknowledgement; an
                MSI-X vector delivered 65 times unacknowledged reads masked (the
                `irq` line); after the last handle to its object closes, the line is
                masked.";
            attribute :>> parent = ("O.CAPABILITY", "O.QUOTA");
        }
        requirement <'H.IRQ.3'> unclaimedLinesMasked : ItemHighLevel {
            attribute :>> statement = "An interrupt on a line no object and no kernel
                handler holds shall be masked and counted, and delivered to nobody.";
            attribute :>> criterion = "A delivery provoked on a line nobody holds
                leaves the line masked and the unclaimed count one higher, with 0
                deliveries (the `edges` line).";
            attribute :>> parent = "O.QUOTA";
        }
    }

    package DirectMemoryAccess {
        doc /* Devices driven from ring 3 through apertures and IOMMU domains:
             * O.DMA and ASR-4 (FDP_ACF.1, FDP_IFF.1). ARMv7-A's reference
             * board has no IOMMU, so these hold on x86-64 and AArch64 only
             * (AoU-6). H.DMA.6 and H.DMA.7 are split from H.DMA.2, and
             * H.DMA.8 from H.DMA.3, so that one check can prove each (W-8's
             * iommu slice). That a pin is made only through a device handle
             * the driver holds is H.OBJ.1 and H.OBJ.2's: vmo_pin is a native
             * call like any other. */

        requirement <'H.DMA.1'> everyFunctionBehindAUnit : ItemHighLevel {
            attribute :>> statement = "Where a translating IOMMU is present, every
                PCI function the kernel enumerates shall be placed behind a unit
                before its driver starts, and a function no unit covers shall be
                reported.";
            attribute :>> criterion = "The boot counts N functions behind units, 0
                bypassing and 0 unresolved (the `iommu` line), with VT-d on x86-64
                and SMMUv3 on AArch64.";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.2'> outOfDomainWritesRefused : ItemHighLevel {
            attribute :>> statement = "A device write to an address outside its
                domain shall be refused by the unit and recorded as a fault against
                the device's stream and the page it addressed.";
            attribute :>> criterion = "A device made to write a page its translated
                domain does not map has the write recorded by its unit as a write by
                its stream to that page: 1 or more out-of-domain writes faulted (the
                `pci` line) on x86-64 and AArch64.";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.6'> noFaultUnprovoked : ItemHighLevel {
            attribute :>> statement = "A boot shall end with no DMA fault recorded
                that no check provoked.";
            attribute :>> criterion = "Last before the success marker, the audit
                reads every translating unit's records and counts 0 faults no check
                provoked, unit events and overflows included (the `iommu` audit
                line).";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.7'> refusedWritesReachNothing : ItemHighLevel {
            attribute :>> statement = "A device write its unit refuses shall reach
                no memory.";
            attribute :>> criterion = "A device made to write a frame the kernel
                holds and its domain does not map leaves every byte of the frame as
                it was.";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.3'> onlyPinnedFramesReachable : ItemHighLevel {
            attribute :>> statement = "A device's domain shall translate a frame's
                address only while the frame is pinned into it, and shall refuse a
                pin of nothing and the unpin of a pin another domain took.";
            attribute :>> criterion = "Pages pinned through a translated domain are
                found at their device addresses while pinned and not after; a pin of
                no frames and an unpin by another domain are refused (the `iommu`
                pin line's 2 refusals as specified).";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.8'> unpinnedBeforeReused : ItemHighLevel {
            attribute :>> statement = "Unpinning shall complete the unit's
                invalidation before the frame or a table the unpin emptied is
                reused, and a unit that never completes it shall leave both held.";
            attribute :>> criterion = "A table an unpin empties returns to the frame
                allocator only after the unit has answered the invalidation that
                covers it, and a unit made never to answer keeps the pin's frames and
                the table out of the allocator.";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.9'> aDeviceRaisesOnlyItsOwnVectors : ItemHighLevel {
            attribute :>> statement = "Where the machine's interrupts are isolated, a
                device shall raise only the interrupt vectors minted for it: a
                message in compatibility format, or naming another function's
                remapping entry, shall be refused, whatever vector or delivery
                mode it asks for.";
            attribute :>> criterion = "On x86-64 under KVM and TCG, edu's forged
                compatibility-format messages to 0xFC and as an NMI, and its message
                naming another function's entry, are refused with faults 0x25 and
                0x26 and arrive nowhere; the console's line is delivered only
                through its own entry; a device marked for isolated interrupts is
                refused vectors while they are not (the remap lines). AArch64 under
                GICv2m, ARMv7-A and a machine without VT-d interrupt remapping are
                its stated residuals.";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DMA.4'> deadDriversPinsQuarantined : ItemHighLevel {
            attribute :>> statement = "Frames a driver pinned shall not be reused
                after the driver dies until its replacement announces itself, and the
                frames so held shall be bounded.";
            attribute :>> criterion = "A dead driver's pin is quarantined, a pin past
                twice the device's pin budget, its quarantine counted, is refused
                and taken again once the quarantine is released, and a live
                driver's pin is given back (the `iommu` pin budget line).";
            attribute :>> parent = ("O.DMA", "O.QUOTA");
        }
        requirement <'H.DMA.5'> registersOnlyThroughApertures : ItemHighLevel {
            attribute :>> statement = "A driver shall reach device registers only
                through an I/O mapping of an aperture it holds, and an aperture that
                is not whole pages or that covers an MSI-X table shall be withheld.";
            attribute :>> criterion = "The `devices` line counts the apertures
                withheld for partial pages and for MSI-X ranges; a mapping of a
                withheld range is refused; a program reaches only the aperture it
                was given (the `handles` line).";
            attribute :>> parent = ("O.DMA", "O.CAPABILITY", "ASR-4");
        }
    }

    package Devices {
        doc /* A device served by its drivers one at a time, and stopped
             * between them: O.CAPABILITY and ASR-3 for the claim, O.DMA and
             * ASR-4 for the quiesce. Proposed 2026-09-27 with part 20's
             * low-level requirements, which nothing above them stated. */

        requirement <'H.DEV.1'> servedByItsClaimsAlone : ItemHighLevel {
            attribute :>> statement = "A device shall be claimed through no more of
                its core's control channels at once than the core allows -- one,
                but for an STM32 USB host's input functions -- and a claim shall
                stand until the core lets it go.";
            attribute :>> criterion = "A second claim of a node is refused, a
                shared node's claims stop at its limit, and one claim let go leaves
                the others (the `services` line).";
            attribute :>> parent = ("O.CAPABILITY", "ASR-3");
        }
        requirement <'H.DEV.2'> refusedUnderALiveDriver : ItemHighLevel {
            attribute :>> statement = "A quiesce shall be refused while a live
                driver serves the device.";
            attribute :>> criterion = "A quiesce of the device a running block
                driver serves answers BAD_STATE (the `blk-ring` line).";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DEV.4'> stoppedBeforeItAnswers : ItemHighLevel {
            attribute :>> statement = "A quiesce shall answer success only once
                every core has let the device go and the function's bus mastering
                is off.";
            attribute :>> criterion = "A quiesce the instant a block driver dies
                answers success, and the function's command register, whose
                bus mastering was turned on while the driver served it as its
                first pin turns it, reads it off (the `blk-ring` line).";
            attribute :>> parent = ("O.DMA", "ASR-4");
        }
        requirement <'H.DEV.3'> aRestartedDriverKeepsItsName : ItemHighLevel {
            attribute :>> statement = "A device a driver started again serves shall
                be published under the lowest number its core has free, so that it
                takes back the name its predecessor had.";
            attribute :>> criterion = "A number given back is the next taken (the
                `services` line); a GPU driver killed and restarted is card0
                again (`xtask test-restart`).";
            attribute :>> parent = "P.2";
        }
    }

    package Traps {
        doc /* The system call boundary and the exceptions a program raises:
             * O.VALIDATE and ASR-7 (FPT_TDC.1), with O.ISOLATE for what a
             * program may leave in the registers the kernel is entered with. */

        requirement <'H.TRAP.1'> userRangesChecked : ItemHighLevel {
            attribute :>> statement = "Every pointer and length a system call is
                given shall be checked to lie wholly within the user half before any
                access through it, and a range that does not shall be refused with
                EFAULT.";
            attribute :>> criterion = "Each system call given a kernel-half pointer,
                a range that wraps past the user half's end and a null pointer is
                refused EFAULT, with 0 accesses made.";
            attribute :>> parent = ("O.VALIDATE", "ASR-7");
        }
        requirement <'H.TRAP.2'> unmappedUserMemoryIsEfault : ItemHighLevel {
            attribute :>> statement = "An access to user memory on a system call's
                behalf that meets an unmapped or protected page shall return EFAULT
                to the caller and shall not fault the kernel.";
            attribute :>> criterion = "Copies to an unmapped and to a read-only user
                page, and from an unmapped one, each return EFAULT, and the boot goes
                on to FERRIX-BOOT-OK.";
            attribute :>> parent = ("O.VALIDATE", "ASR-7");
        }
        requirement <'H.TRAP.3'> indicesClampedUnderSpeculation : ItemHighLevel {
            attribute :>> statement = "Every table index a program chooses shall be
                clamped at the system call boundary so that it cannot be used out of
                bounds under speculation, and each processor's speculation controls
                shall be applied and read back.";
            attribute :>> criterion = "The boot reads the speculation defences back
                on every one of the N online processors (the `cpu` line), and the
                `--mitigations off` build is the only one without the clamps.";
            attribute :>> parent = ("O.ISOLATE", "O.VALIDATE");
        }
        requirement <'H.TRAP.4'> userFaultsBecomeSignals : ItemHighLevel {
            attribute :>> statement = "A processor exception a user-mode instruction
                raises shall end in a signal to the faulting thread, the one Linux
                sends for it, and shall not stop the kernel.";
            attribute :>> criterion = "A divide error, an invalid opcode, an unmapped
                read, a privileged instruction, a floating-point exception and a read
                past a mapped file's end end their programs with SIGFPE, SIGILL,
                SIGSEGV, SIGSEGV, SIGFPE and SIGBUS, 6 of 6 (the `fault` line).";
            attribute :>> parent = ("O.ISOLATE", "ASR-1");
        }
        requirement <'H.TRAP.5'> signalReturnCannotRaisePrivilege : ItemHighLevel {
            attribute :>> statement = "Restoring a signal frame a program supplies
                shall not raise its privilege: the restored mode, segment selectors,
                interrupt masks and I/O privilege shall be the user's whatever the
                frame holds, and a return address outside the user half shall be
                refused.";
            attribute :>> criterion = "A sigreturn through frames asking for kernel
                mode, masked interrupts, I/O privilege and a kernel-half return
                address resumes the program in user mode with user flags, or ends it
                with SIGSEGV, on each architecture: 0 in kernel mode.";
            attribute :>> parent = ("O.VALIDATE", "O.ISOLATE", "ASR-7");
        }
        requirement <'H.TRAP.6'> entryTrustsNoUserState : ItemHighLevel {
            attribute :>> statement = "On every entry from user mode the kernel shall
                establish its own stack and per-processor state before using either,
                whatever the program left in its registers, including for an
                exception taken on the entry path itself.";
            attribute :>> criterion = "Breakpoints on the system call entry and on
                its return, and an NMI taken with interrupts masked, each run on the
                kernel's own stack with its own per-processor base, and the program
                exits 42 (the `debug` and `nmi` lines, x86-64).";
            attribute :>> parent = ("O.ISOLATE", "O.FAILSAFE");
        }
        requirement <'H.TRAP.7'> onlyUserSelectors : ItemHighLevel {
            attribute :>> statement = "A program shall load only the segment
                selectors meant for user mode: one naming a kernel descriptor, the
                TSS, the LDT or a forged thread-local descriptor shall be refused.
                x86-64 only.";
            attribute :>> criterion = "7 user selectors load at RPL 3 and 18 others
                are refused (the `gdt` line).";
            attribute :>> parent = ("O.ISOLATE", "O.VALIDATE");
        }
        requirement <'H.TRAP.8'> compatibilityModeAsLinux : ItemHighLevel {
            attribute :>> statement = "A 32-bit x86 program shall run in ring 3 in
                compatibility mode, in Linux's user segments and its own
                thread-local descriptors, and shall enter the kernel only through
                int $0x80, answered as i386 Linux answers it. x86-64 only.";
            attribute :>> criterion = "An ELF32 i386 program's calls through int
                $0x80 answer as i386 Linux's do and it exits 42; a 64-bit program's
                int $0x80 is an i386 call; SYSCALL from 32-bit code answers -ENOSYS
                or is #UD, and SYSENTER ends the program (the first `i386` line).";
            attribute :>> parent = ("O.ISOLATE", "G.5");
        }
        requirement <'H.TRAP.9'> signalFramesAsLinux : ItemHighLevel {
            attribute :>> statement = "A 64-bit program's signal handler shall be
                entered on a frame in Linux's layout, and its return shall resume
                the registers and mask the frame holds.";
            attribute :>> criterion = "The 64-bit signal program's SA_SIGINFO
                handler is entered with the signal, the siginfo and the ucontext, a
                register it changes through the frame comes back through
                rt_sigreturn, and the mask after the return is empty: exit 77 (the
                `signals` line).";
            attribute :>> parent = "G.3";
        }
        requirement <'H.TRAP.10'> registersAsTheAbiSays : ItemHighLevel {
            attribute :>> statement = "A program's registers shall pass the kernel
                as its ABI says: a call's arguments are read from its argument
                registers and its result returned in its return register, a fork
                child resumes from its parent's registers with the call answering
                zero, and a program started or replaced begins at its entry with its
                start argument and no kernel value in any register.";
            attribute :>> criterion = "A call with six arguments answers from all
                six; a fork child's registers are its parent's with the return
                register zero; a program started with an argument finds it, and one
                that ORs its registers on entry finds nothing else set, for each
                ABI.";
            attribute :>> parent = ("G.4", "G.5");
        }
        requirement <'H.TRAP.11'> unknownCallsAreEnosys : ItemHighLevel {
            attribute :>> statement = "A system call number no table carries shall
                be refused with ENOSYS, never answered by a handler or a fault.";
            attribute :>> criterion = "0xDEAD, usize::MAX and usize::MAX - 1, each
                dispatched as a native call through the registered entry, are
                answered -38, 3 of 3.";
            attribute :>> parent = "O.VALIDATE";
        }
        requirement <'H.TRAP.12'> decodedByTheEntrysTable : ItemHighLevel {
            attribute :>> statement = "A system call's number shall be decoded
                through the table of the architecture it was built for.";
            attribute :>> criterion = "Of getpid's numbers in the three tables,
                exactly the running architecture's decodes to getpid, and
                dispatching it answers a pid above zero.";
            attribute :>> parent = "O.VALIDATE";
        // H.TRAP.13 to H.TRAP.15 were added by W-8 step 4 (console): the kernel
        // log, which syslog(2) and a log control channel read (T.CONFUSE path 6
        // of VULNERABILITY-ANALYSIS.md), and a program's output on the console.
        requirement <'H.TRAP.13'> logReadsStayInTheLog : ItemHighLevel {
            attribute :>> statement = "A read of a log ring -- the kernel log is one
                -- shall copy into the reader's buffer, up to its length, only bytes
                the ring recorded, in order from the reader's cursor or from the
                oldest byte kept if that is later, and shall count as lost exactly
                the bytes it skips or finds replaced while it copied.";
            attribute :>> criterion = "On a 64-byte ring of the log check's own given
                100 bytes, a reader at 0 is told 37 lost and given the last 63 in
                order across a 10-byte read and a longer one, and 200 bytes later is
                told 137 lost and given 63; while two writers record 20,000 bytes each
                into a 256-byte ring, a reader reads only bytes they wrote, and the
                bytes it read and was told it lost come to exactly 40,000 (the `log`
                line).";
            attribute :>> parent = ("O.VALIDATE", "O.ISOLATE");
        }
        requirement <'H.TRAP.14'> programOutputAsWritten : ItemHighLevel {
            attribute :>> statement = "A program's write to the console shall reach
                the port byte for byte in the order written, each bare newline as CRLF
                unless the program turned output processing off.";
            attribute :>> criterion = "A write of text holding bare newlines reaches
                the port with CRLF for each, and the same write with output processing
                off with LF alone, every other byte the same and in order.";
            attribute :>> parent = "G.5";
        }
        requirement <'H.TRAP.15'> theLogHoldsWhatWasSent : ItemHighLevel {
            attribute :>> statement = "The kernel log shall hold every byte the
                console is given to send, a kernel line or a program's output, as it
                was written and before a newline becomes CRLF, except the lines the
                kernel sends unlogged.";
            attribute :>> criterion = "Over a boot, the kernel log read from its
                oldest byte holds, in order and without CRs, every kernel line the
                serial log holds but the unlogged ones, and every program's output.";
            attribute :>> parent = "G.5";
        }
        requirement <'H.TRAP.16'> aRegisteredFilterOnlyTakesAwayFromACall : ItemHighLevel {
            attribute :>> statement = "A look a personality registers with the core at
                its system call entries, before any answer of the entry's own, shall be
                able to make a call do only less than it could: let it go on, or fail it
                with an errno the core bounds to 0 to 4095. It shall have no way to
                answer with anything else, so that it cannot start a program or choose
                what a program's registers hold; a filter that must end the program does
                so through the paths exit and a fatal signal already take. It shall be
                asked about every call of every entry
                once, first, with the entry's own architecture token and the
                instruction after the call, and nothing registered shall change what
                the core does.";
            attribute :>> criterion = "Driven through each entry this architecture
                has, every call the entry answers itself reaches a test-only filter
                once, before that answer, and the filter's value is the one the program
                gets; an int $0x80 call carries i386's token; a number with bits above
                the 32nd set is judged as its low half and dispatched as no call; a
                native-range call carries a token of its own; an errno a filter answers is
                cut to 4095 by the core, and 512 is an ordinary errno (the `seccomp`
                line).";
            attribute :>> parent = ("O.ISOLATE", "O.VALIDATE");
        }
        requirement <'H.TRAP.17'> barrierOutsideASpeculationDomain : ItemHighLevel {
            attribute :>> statement = "A switch between the address spaces of two
                programs not in one speculation domain shall issue the predictor
                barrier; a switch between two address spaces of one domain shall not.
                A domain is a job marked at its creation by the holder of MANAGE on
                its parent, and holds only the programs born in it that have not
                left it.";
            attribute :>> criterion = "The `domain` line: switches inside one
                domain decide no barrier, and switches to and from no domain,
                another domain, a domain inside it, a mover, a leaver and a sharer
                decide one each way, on every architecture the suite boots.";
            attribute :>> parent = "O.ISOLATE";
        }
    }

    package Boot {
        doc /* What the kernel does with what it is handed before anything runs
             * above it, and what a boot claims: O.FAILSAFE and ASR-6 (FPT_FLS.1),
             * and O.ISOLATE for the layout. */

        requirement <'H.BOOT.1'> handOffVerified : ItemHighLevel {
            attribute :>> statement = "Before using what the loader hands over, the
                kernel shall verify the memory map, the page tables and its own
                placement, and shall halt with a diagnostic if any check fails.";
            attribute :>> criterion = "Stage 1 reports the hand-off verified on
                every architecture; a hand-off with a field made inconsistent ends in
                the catalogued panic, not in a boot.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        requirement <'H.BOOT.2'> aBootSaysWhetherItChecked : ItemHighLevel {
            attribute :>> statement = "Every boot shall run every stage's self-checks
                unless told not to, and shall print FERRIX-BOOT-OK only when all of
                them passed; a boot that skipped them shall end with
                FERRIX-BOOT-UNCHECKED instead.";
            attribute :>> criterion = "`cargo xtask test-boot` finds FERRIX-BOOT-OK
                on each architecture; a boot with `ferrix.checks=skip` ends with
                FERRIX-BOOT-UNCHECKED and never prints FERRIX-BOOT-OK.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        requirement <'H.BOOT.3'> layoutIsRandomised : ItemHighLevel {
            attribute :>> statement = "The kernel shall run with its image, direct
                map and vmap arena at the offsets the loader drew from the firmware's
                random-number protocol, and shall verify the move at stage 1.";
            attribute :>> criterion = "Two boots of one image report two different
                layouts (`cargo xtask test-kaslr`), and stage 1 verifies the slide
                each reports.";
            attribute :>> parent = "O.ISOLATE";
        }
        requirement <'H.BOOT.4'> firmwareTablesBounded : ItemHighLevel {
            attribute :>> statement = "The kernel shall refuse an ACPI table or a
                device tree whose declared length is shorter than its header or
                reaches beyond the memory mapped for it, rather than read past it.";
            attribute :>> criterion = "The ACPI and device-tree parsers' host tests
                feed truncated, oversized and misaligned tables and each is refused
                with its error, 0 read out of bounds; the kernel refuses a table
                outside the direct map.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        // Added by W-8 step 4 (smp): the processors a boot claims, which no
        // requirement above said.
        requirement <'H.BOOT.5'> everyProcessorComesUp : ItemHighLevel {
            attribute :>> statement = "Every processor firmware describes shall be
                running kernel code on its own per-processor record before the
                scheduler starts, or the boot shall halt with a diagnostic saying
                which step failed.";
            attribute :>> criterion = "On each configuration `cargo xtask test-boot`
                boots, the `cpus` line counts as many processors online as firmware
                described (4 of 4 on x86-64 and AArch64, 2 of 2 on ARMv7-A at `--smp
                2`); a secondary made never to report in ends the boot in FX-0402.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        requirement <'H.BOOT.6'> endsPoweredOff : ItemHighLevel {
            attribute :>> statement = "When init exits and ferrix.onexit asks for
                nothing else, the machine shall power itself off after the console
                has drained.";
            attribute :>> criterion = "After init exits, QEMU exits by itself within
                the grace, with status 0 or 33, and init's exit line is in the log.";
            attribute :>> parent = "O.FAILSAFE";
        }
        requirement <'H.BOOT.7'> resetsWhenAsked : ItemHighLevel {
            attribute :>> statement = "When init exits and ferrix.onexit=reset asks
                for it, the machine shall restart from firmware after the console
                has drained.";
            attribute :>> criterion = "Under ferrix.onexit=reset the kernel says it
                is resetting and the loader's banner is printed again after it.";
            attribute :>> parent = "O.FAILSAFE";
        }
        requirement <'H.BOOT.8'> consoleCarriesTheSession : ItemHighLevel {
            attribute :>> statement = "The serial console shall carry the kernel's
                lines out, and a person's typed bytes in to the program reading
                them, in order and without waiting unboundedly on the port.";
            attribute :>> criterion = "test-boot reads every stage's line and
                FERRIX-BOOT-OK from the serial port, and test-jobs's 18 typed steps
                each have their answer after them.";
            attribute :>> parent = "O.FAILSAFE";
        // Added by W-8 step 4 (console): the KASLR half of F-31 is worth only as
        // much as nothing a program reads gives the layout away, and the kernel
        // log is readable (console::write_unlogged says which lines stay out).
        requirement <'H.BOOT.9'> theLogHoldsNoLayout : ItemHighLevel {
            attribute :>> statement = "The kernel log shall hold neither the KASLR
                slide nor any kernel virtual address the kernel prints: a panic's
                slide and backtrace, a fatal trap's registers, stage 2's placement of
                the image, the direct map and the page array, and the sweeps' failure
                lines shall go to the port and never into the log.";
            attribute :>> criterion = "After a boot, and after a panic with a
                backtrace, the kernel log read whole holds 0 occurrences of the slide
                or of any image, direct-map or vmap address the serial log printed.";
            attribute :>> parent = "O.ISOLATE";
        }
        requirement <'H.BOOT.15'> pid1IsWhatTheImageNames : ItemHighLevel {
            attribute :>> statement = "Pid 1 shall be the program the boot names: the
                file ferrix.init= names, else the commands or the program the boot
                initramfs carries under .ferrix/init, else /sbin/init; an input that
                cannot be one shall be refused with a line saying which and why,
                and never replaced by a guess.";
            attribute :>> criterion = "Each way of starting pid 1 boots as named
                (test-shell, test-vfs, test-init); malformed inputs are each refused
                and taken as absent (L.init.1 to L.init.3); and none is unpacked
                into a root, which SAFETY-MANUAL AoU-24 assumes of the unpacker.";
            attribute :>> parent = "O.FAILSAFE";
        }
    }

    package Quotas {
        doc /* What a job, with every job beneath it, may hold at once:
             * O.QUOTA (FRU_RSA.1), against T.EXHAUST. */

        requirement <'H.QUOTA.1'> taskLimit : ItemHighLevel {
            attribute :>> statement = "Creating a task in a job that, with the jobs
                beneath it, holds its task limit shall be refused, and no task shall
                be counted beyond the limit.";
            attribute :>> criterion = "A fork loop in a job limited to 8 tasks is
                refused at the limit, 0 tasks beyond it (the `quota` line).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.2'> memoryLimit : ItemHighLevel {
            attribute :>> statement = "A job's programs shall not hold more memory
                than the job's limit, counting the frames of their address spaces
                and their page tables together with every job beneath it; a sibling
                job shall be unaffected.";
            attribute :>> criterion = "Faults in a job limited to 48 pages' worth
                are refused when the next page would not fit, and the refusal is
                counted, while a sibling job faults 48 pages in (the `quota`
                line).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.3'> objectLimit : ItemHighLevel {
            attribute :>> statement = "Creating a channel or a port in a job at its
                object limit shall be refused, and no object shall be counted beyond
                the limit.";
            attribute :>> criterion = "Object creation in a job limited to 5 objects
                is refused at the limit, 0 objects beyond it (the `quota` line).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.4'> chargesComeBack : ItemHighLevel {
            attribute :>> statement = "Every charge against a job shall be returned
                when the resource it paid for is freed, and a job's quota slot shall
                be given back when the job goes.";
            attribute :>> criterion = "After the quota checks every counter of their
                jobs is back to zero, and as many quota slots are in use as before
                them (the `quota` line).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.5'> limitsAreHierarchical : ItemHighLevel {
            attribute :>> statement = "A limit set on a job shall bound it together
                with every job beneath it, and a charge refused at any level shall
                leave no level charged.";
            attribute :>> criterion = "A charge that would pass a grandparent's limit
                is refused although the child's own limit allows it, and each level's
                counter reads as before the attempt.";
            attribute :>> parent = "O.QUOTA";
        }
        // Added by the low-level pilot on object/ (W-8 step 3), like H.OBJ.10-13.
        requirement <'H.QUOTA.6'> oomKillIsScoped : ItemHighLevel {
            attribute :>> statement = "A user page fault refused by a job's memory
                limit shall end a process in the job whose limit was full or beneath
                it, never one outside it, count the kill in that job and every job
                above it, and retry the fault.";
            attribute :>> criterion = "A program writing 8 MiB in a job limited to 1
                MiB ends by SIGKILL while a larger process in a sibling job lives;
                exactly 1 process is killed; memory.events counts one OOM kill in the
                job and its parent and none in the sibling.";
            attribute :>> parent = ("G.8", "O.QUOTA");
        }
        // Split out of H.QUOTA.2 and H.QUOTA.4 by the pilot, for one check each.
        requirement <'H.QUOTA.7'> kernelHeapIsCharged : ItemHighLevel {
            attribute :>> statement = "The kernel heap the Linux personality keeps
                for a job's programs shall be charged against the job's memory
                limit, and every byte of it returned when it is freed; a sibling job
                shall be unaffected.";
            attribute :>> criterion = "At a 32 KiB limit each kind of personality
                object is refused one past its count with ENOMEM (ENOLCK for a lock)
                while a sibling makes one, and with them gone every byte charged has
                come back (the `kmem` line).";
            attribute :>> parent = "O.QUOTA";
        }
        // Split out of H.QUOTA.1 and H.QUOTA.3 by the pilot: the part no check proves.
        requirement <'H.QUOTA.8'> taskLimitIsAnswered : ItemHighLevel {
            attribute :>> statement = "A creation the task limit refuses shall be
                answered as Linux and the native ABI answer it -- a fork with
                EAGAIN, a native process_create with SHOULD_WAIT -- and the refused
                process shall never start.";
            attribute :>> criterion = "A fork past pids.max is answered EAGAIN and
                counted in pids.events; process_create into a job at its task limit
                is answered SHOULD_WAIT and the job's task count is unchanged.";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.9'> everyObjectIsCounted : ItemHighLevel {
            attribute :>> statement = "Creating a VMO, a job or a pin in a job at
                its object limit shall be refused with NO_MEMORY, and each shall be
                counted as an object for as long as it exists.";
            attribute :>> criterion = "In a job limited to 5 objects and holding 5,
                vmo_create, job_create and vmo_pin are each refused NO_MEMORY, 0
                objects counted beyond the limit, and the job holds 0 once they are
                all closed.";
            attribute :>> parent = "O.QUOTA";
        }
        // Stage 13's controllers (cpu.max, cgroup.freeze, reclaim), 2026-10-01.
        requirement <'H.QUOTA.10'> cpuBandwidthIsBounded : ItemHighLevel {
            attribute :>> statement = "A job's tasks, with those of every job
                beneath it, shall use no more processor time in a period than the
                job's cpu.max allows, and a task kept waiting for the period to
                end shall leave the wait at once for a kill or a signal.";
            attribute :>> criterion = "A program in a job limited to 20000 of
                100000 microseconds is held to between a twelfth and two thirds of
                a processor with the throttling counted in cpu.stat, and one in a
                job limited to 1000 of 1000000 ends within a quarter of a second
                of its SIGKILL (the `cgroups` line, cpu).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.11'> aFrozenJobStops : ItemHighLevel {
            attribute :>> statement = "A process in a job, or beneath one, whose
                cgroup.freeze is set shall run no user code until the job is
                thawed or the process is killed; SIGCONT shall not thaw it, and
                cgroup.events shall say frozen only once every live process
                beneath has stopped.";
            attribute :>> criterion = "A program of three threads in a frozen
                cgroup stops counting, cgroup.events says frozen 1 and wakes its
                poller, SIGCONT changes neither, and thawing restarts its futex
                wait (the `cgroups` line, freeze).";
            attribute :>> parent = "O.QUOTA";
        }
        requirement <'H.QUOTA.12'> reclaimComesBeforeTheKill : ItemHighLevel {
            attribute :>> statement = "A charge refused by a job's memory limit,
                and a program over its memory.high, shall first take back clean
                file cache charged to that job or beneath it, never a sibling's
                and never a child's at or under its memory.min, and read back as
                its source has it, before anything is killed.";
            attribute :>> criterion = "A cgroup reading a file twice its
                memory.high is brought back to it, its sibling's cache untouched,
                every page read back as the source has it, and a read at
                memory.max kills nothing where its clean cache was room (the
                `cgroups` line, reclaim).";
            attribute :>> parent = ("G.8", "O.QUOTA");
        }
    }

    package Audit {
        doc /* The audit record of the TSF's own decisions: O.AUDIT (FAU_GEN.1,
             * FAU_GEN.2, FAU_SAR.1, FAU_SAR.2, FAU_STG.1, FAU_STG.4), for
             * P.ACCOUNTABILITY. docs/certification/AUDIT.md is the design. */

        requirement <'H.AUD.1'> gaplessAndStarted : ItemHighLevel {
            attribute :>> statement = "Each audit ring shall number its records
                from zero without a gap, the high-value ring's first being the
                start-up record with the boot's whole audit id and both ring
                lengths; a second start shall change nothing, and a reader asking
                for a record the ring overwrote shall be moved on and told exactly
                how many it lost.";
            attribute :>> criterion = "On a store of four high-value records, the
                start-up record is number 0 and carries the id and lengths; a
                second start is refused; after six grants a read from 0 copies
                records 3 to 6 and reports 3 lost; a read of two from 3 resumes at
                5.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.2'> theBootsRecordsPinned : ItemHighLevel {
            attribute :>> statement = "The first eight system records shall stay
                readable, numbered as the high-value ring numbered them, however
                far that ring has wrapped, and no record after them shall join
                them.";
            attribute :>> criterion = "Nine system records and a grant in a ring
                of four: the boot records read back 0, 1, 2, 3, 5, 6, 7, 8, the
                ring reports records lost, and a read of the boot records from 6
                copies 3 and resumes at 9.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.3'> refusalsKeptApart : ItemHighLevel {
            attribute :>> statement = "A refusal shall be kept in the refusal ring
                and a record of every other class in the high-value ring, so that
                no flood of refusals can overwrite a grant.";
            attribute :>> criterion = "One record of each of the six classes: the
                refusal alone in the refusal ring, the other five in the
                high-value ring in the order made.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.4'> aBudgetIsItsMakers : ItemHighLevel {
            attribute :>> statement = "A job shall be charged, for the fairness of
                the refusal ring, to the budget its maker gave it: its parent's
                for a job its members could make themselves, its own otherwise.";
            attribute :>> criterion = "An anonymous job and one inside it are
                charged to their unit, a cgroup made in a delegated unit to that
                unit, and a cgroup root made in a unit to itself: 6 jobs' budgets
                as given.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.5'> aFloodIsCounted : ItemHighLevel {
            attribute :>> statement = "Past 64 refusals charged to one budget in a
                second, the TSF shall count that budget's further refusals rather
                than keep them, and write the count once the second has ended,
                while another budget's refusals and every grant are kept.";
            attribute :>> criterion = "104 refusals from a unit's three jobs keep
                64 between them and, once the second has ended, one suppressed
                record counting 40 under the unit; the delegated unit's refusal in
                the same second is kept, and a grant made before is still read
                with none lost.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.6'> anEvictedCountIsWritten : ItemHighLevel {
            attribute :>> statement = "A budget whose fairness window is taken to
                make room for another shall have its count written, not
                dropped.";
            attribute :>> criterion = "A budget one over its limit, then 32 more
                budgets: the first's window is closed and its suppressed record
                counts 1.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.7'> theConfigurationRecorded : ItemHighLevel {
            attribute :>> statement = "The boot shall start its audit record with
                the start-up record of the id reads answer, and record whether its
                self-checks run, who starts devmgr, the mitigations it was built
                with and the KASLR state, as bring-up read them.";
            attribute :>> criterion = "The kernel's boot records begin with the
                start-up record, its id the one reads answer and its lengths 512
                and 4096, and hold the 4 configuration records with bring-up's
                values; the boot is not yet recorded as brought up.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.8'> everyDecisionRecorded : ItemHighLevel {
            attribute :>> statement = "Each decision the TSF makes at a recording
                site -- a call its rights refused, a widening refused, a limit's
                refusal, a process made, a job given for a cgroup, a control
                channel given, a job killed, a cgroup killed, an OOM kill, a device
                quiesced, a limit set by handle or by cgroup file, a DMA fault --
                shall be recorded with its outcome and the subject that made it,
                the kernel for the decisions it makes alone.";
            attribute :>> criterion = "At the end of a boot with checks, the
                record holds each of the 12 events the boot's checks provoke, and
                DMA_FAULT where a unit reported a DMA fault, with the outcome of its class
                and its subject; none of the OOM kill, the quiesce and the fault
                names a process; the boot is recorded as brought up.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.9'> readByItsHandleAlone : ItemHighLevel {
            attribute :>> statement = "The audit record shall be read only through
                the audit handle with READ, which can be neither duplicated nor
                sent; a read shall copy whole records from a cursor, and a read
                that copies none or cannot copy them out shall leave the cursor,
                the buffer and the reader's position where they were.";
            attribute :>> criterion = "A handle with pid 1's rights reads the boot
                records, the start-up record first with the kernel's id; a count
                of 0 copies nothing and moves nothing; a read into unmapped memory
                is FAULT and counts nothing read; a handle without READ,
                a channel end and a ring that does not exist are refused; the
                handle is not duplicated and not sent.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.10'> keptOnTheVolume : ItemHighLevel {
            attribute :>> statement = "A boot with init shall keep its audit record
                on the root volume in a file named by the boot's audit id, holding
                the start-up record, the configuration, and the decisions only a
                boot with init makes.";
            attribute :>> criterion = "svc audit reads back the start-up record
                with the id init named the file by, the checks' configuration as
                run, STARTER_GIVEN, READER_GIVEN, DEVMGR_STARTED by pid 1 and
                ROOT_SWITCHED with pid 1 moved.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.11'> nothingLostAtPowerOff : ItemHighLevel {
            attribute :>> statement = "Every record up to a power action shall be
                in the reader's file or on the console: the kernel shall record the
                power action last, say how far the reader read, and print every
                record past that, and a run the ring overwrote as one line.";
            attribute :>> criterion = "At test-init's power-off, init's last read
                is where the kernel saw it stop, and every number from there to
                the power action is printed as unread or in a lost run.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.12'> anUncheckedBootSaysSo : ItemHighLevel {
            attribute :>> statement = "A boot that skips its self-checks shall
                record that it did, where a reader outside it can see.";
            attribute :>> criterion = "A boot with ferrix.checks=skip, read back
                through svc audit, holds the checks' configuration record with
                value 0.";
            attribute :>> parent = "O.AUDIT";
        }
        requirement <'H.AUD.13'> aRecordsLayout : ItemHighLevel {
            attribute :>> statement = "An audit record shall be 64 bytes, each
                field little-endian at a fixed offset, so that a reader decodes
                what the kernel wrote.";
            attribute :>> criterion = "A record of distinct bytes encodes to them
                in order at the offsets the layout names and decodes back equal.";
            attribute :>> parent = "O.AUDIT";
        }
    }

    package Failure {
        doc /* The safe state and what brings the kernel to it: O.FAILSAFE and
             * ASR-6 (FPT_FLS.1). SAFETY-MANUAL.md §3 defines the safe state. */

        requirement <'H.FAIL.1'> haltOnInconsistency : ItemHighLevel {
            attribute :>> statement = "On a failed internal consistency check the
                kernel shall stop every other processor, print a diagnostic naming the
                failure and its catalogued explanation, and halt without returning to
                user mode.";
            attribute :>> criterion = "A boot told to panic at its end
                (`ferrix.onexit=panic`) ends with the FX-1501 report and no program
                output after it; every panic site in the item has a catalogue entry
                (`check-panic-audit.py`).";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        requirement <'H.FAIL.2'> stacksAreGuarded : ItemHighLevel {
            attribute :>> statement = "The allocator every kernel stack comes from
                shall give each an unmapped guard page directly below and above
                it.";
            attribute :>> criterion = "A kernel stack allocated, written at its first
                and last word and freed has the page below and the page above it
                untranslated, and gives back every frame it took.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        // Split out of H.FAIL.2 by W-8 step 4 (mm and user): the half no check proves.
        requirement <'H.FAIL.3'> overflowIsReported : ItemHighLevel {
            attribute :>> statement = "A kernel stack overflow shall end in the halt
                and diagnostic of the safe state, never in a write below the stack.";
            attribute :>> criterion = "A kernel thread that recurses past its stack
                ends the boot in a report naming the fault -- on x86-64 a double fault
                taken on its own stack (FX-9004) -- on each architecture, 3 of 3.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
        // Added by W-8 step 4 (console): the port every diagnostic above is
        // printed on, when whatever holds its lock never lets go.
        requirement <'H.FAIL.4'> reportsGetPastAHeldLock : ItemHighLevel {
            attribute :>> statement = "Once a failure report has begun, a line the
                kernel writes shall reach the console port after a bounded wait even
                when the port's lock is held and will never be released.";
            attribute :>> criterion = "A report begun while another context holds the
                port's lock for good reaches the port whole, after at most 10,000,000
                attempts at the lock, on each architecture, 3 of 3.";
            attribute :>> parent = ("O.FAILSAFE", "ASR-6");
        }
    }

    package Storage {
        doc /* The btrfs reader and write path, `ferrix-btrfs` and
             * `ferrix-btrfs-write`, in the item since 2026-10-02: O.MEDIA and
             * ASR-9 (FPT_TDC.1, FDP_SDI.2). Their interface below is the
             * `Device` and `WriteDevice` traits the kernel's block layer
             * answers, above it `Volume` and `WriteVolume`; the VFS glue on top
             * (`ferrix-btrfs-vfs`, src/kernel/src/fs/btrfs*.rs) is load, so
             * these promise what the two crates do at `Volume` and
             * `WriteVolume`, not what a program sees through a mount. The
             * low-level requirements are part 24. Some sit under a requirement
             * here by decomposition rather than as a case of its criterion:
             * L.btrfs.31 and .70 (a node's address and generation) refine
             * H.STORE.2, whose criterion tests a data sector, and L.btrfs.16
             * and .92 (superblock stripes) refine H.STORE.5, whose criterion
             * tests the trees' consistency. */

        requirement <'H.STORE.1'> untrustedVolumesAnswered : ItemHighLevel {
            attribute :>> statement = "The btrfs reader and writer shall answer
                every volume image, whatever its bytes, with a value or an error,
                without reading or writing outside a buffer, panicking or looping
                without bound.";
            attribute :>> criterion = "Each of 4,096 single-byte patches of a
                superblock, and of a leaf, parses or is refused, and every accessor of
                what parses answers, 0 panics. The decoders' own totality, and the
                `btrfs_read` fuzzer's resealed images, are their low-level
                requirements' and outside this criterion; the write path, which
                parses every node through the reader, has no hostile-image check of
                its own yet (TODO.md).";
            attribute :>> parent = "O.MEDIA";
        }
        requirement <'H.STORE.2'> damageNotReturnedAsData : ItemHighLevel {
            attribute :>> statement = "A block whose checksum disagrees with its
                contents shall never be returned as valid: the reader shall read
                another copy where the chunk keeps one, and otherwise answer an
                error.";
            attribute :>> criterion = "With the first copy of a DUP data sector damaged,
                a file read returns the file's bytes from the second; with both
                damaged, it answers DataChecksum and hands back none of the
                sector's bytes.";
            attribute :>> parent = ("O.MEDIA", "ASR-9");
        }
        requirement <'H.STORE.3'> aCommitIsAllOrNothing : ItemHighLevel {
            attribute :>> statement = "Until a commit's superblock is written, the
                volume on the device shall be exactly the last commit, whatever of
                the commit's other writes have reached it.";
            attribute :>> criterion = "500 inserts committed, and the commit's
                writes replayed without its superblocks onto the volume as it was, leave a volume
                that checks clean and holds exactly the last commit's items.";
            attribute :>> parent = "ASR-9";
        }
        requirement <'H.STORE.4'> promisesSurviveAPowerCut : ItemHighLevel {
            attribute :>> statement = "What a commit or a log commit returned success
                for shall be found, no older, when the volume is opened after a power
                cut at any later point, the log replayed if the superblock names
                one.";
            attribute :>> criterion = "Of 100 cuts over 4 random scenarios of writes,
                logs and commits, each rebuilt as the device may have left the disk,
                every one opens, checks clean down to free space and checksums, and
                rolls back no completed promise.";
            attribute :>> parent = "ASR-9";
        }
        requirement <'H.STORE.5'> nothingInUseIsHandedOut : ItemHighLevel {
            attribute :>> statement = "Each commit shall leave the extent tree, the
                block groups' usage, the free-space tree and the superblock's totals
                agreeing with the trees, so that no block in use is free.";
            attribute :>> criterion = "After 3,000 random edits on the blank
                fixture, committed as they go, the writer's checker finds every tree block's
                and data extent's references, each block group's usage and free
                space and the superblock's total consistent.";
            attribute :>> parent = "ASR-9";
        }
        requirement <'H.STORE.6'> aFullVolumeRefusesBeforeItChanges : ItemHighLevel {
            attribute :>> statement = "An operation the volume has no room for shall
                be refused with no space before it changes anything, leaving the
                transaction able to commit.";
            attribute :>> criterion = "On a 128 MiB volume, creates until its trees
                are full end in NoSpace with the transaction not aborted, its commit
                succeeds, and a deletion still makes room for another create.";
            attribute :>> parent = "ASR-9";
        }
        requirement <'H.STORE.7'> allocationFailureIsAnswered : ItemHighLevel {
            attribute :>> statement = "An allocation the btrfs writer cannot make shall
                be answered to its caller as an error, aborting the transaction if an
                edit had begun, and never as a panic or a stop of the element.";
            attribute :>> criterion = "With every allocation of the writer's
                operations failed in turn, each operation succeeds or answers an
                error, no panic is taken, and the volume reopened from the device is
                the last commit and checks clean.";
            attribute :>> parent = ("O.QUOTA", "ASR-9");
        }
        requirement <'H.STORE.8'> aFailedTransactionIsNeverCommitted : ItemHighLevel {
            attribute :>> statement = "An operation that fails after it changed
                something shall abort its transaction, so that none of it is
                committed, and the volume reloaded at its last commit shall take no
                further change.";
            attribute :>> criterion = "Through a mount, a commit whose device write
                fails answers EIO; the mount then reads the last commit, has no
                trace of the aborted transaction, answers EROFS to a write, a
                truncation and a create, and the disk holds the last commit.";
            attribute :>> parent = "ASR-9";
        }
    }
}
