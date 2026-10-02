/*
 * Ferrix — SysML v2 model, part 16: the low-level requirements of iommu.
 *
 * What each unit of src/kernel/src/iommu.rs and src/kernel/src/iommu/ does, as
 * `ItemLowLevel` requirements (part 13 defines the format): where firmware
 * puts each PCI function's DMA, the units the kernel turns translation on
 * for, the domains a driver pins pages into, the gate a wait on a unit is
 * made through, and the faults a unit records. The pins a program makes
 * through a handle, and the quarantine a dead driver's pins go to, are
 * object/'s (part 14, `L.object.45` to `L.object.49`); this is the domain
 * side under them.
 *
 * The format is the object/ pilot's (W-8 step 3), and so are its rules: a
 * requirement names every function that carries its behaviour as its `unit`;
 * one check proves a whole requirement, so a requirement is split where the
 * checks that exist each prove a part; a statement says no more than its
 * criterion tests; an accessor needs no requirement of its own.
 *
 * What proves what. The boot's own checks are in iommu/check.rs (the domain
 * check, the fault audit, the unit gate), service_check.rs (a fault's words,
 * a second domain, a dropped domain) and object/check.rs (a page pinned
 * twice). Most of what a unit does is proved only end to end, by the
 * out-of-domain probe in pci/virtio.rs: its device writes a page its domain
 * does not map, and it counts the write as faulted only when the unit's
 * record reads back as exactly that write by the domain's stream.
 * pci/virtio.rs is product code, so the tag is on what judges the count,
 * `xtask test-boot`'s `fault_problem`, and on x86-64 and AArch64 only: ARMv7-A
 * programs no unit (AoU-6). What no check proves waits in the baseline as
 * check-writing work; docs/certification/IMPLEMENTATION.md W-8 lists it.
 *
 * Ids are `L.iommu.<n>`, numbered through the subsystem; the package gives
 * the part of it.
 */
package FerrixIommuRequirements {
    private import FerrixItemRequirements::*;

    package Discovery {
        doc /* src/kernel/src/iommu.rs, the first half: every unit firmware
             * describes, and where it puts each PCI function's DMA. Read
             * apart from bring-up, which programs units, and printed on every
             * boot whether or not the checks run. */

        requirement <'L.iommu.1'> unitsAreFoundOnce : ItemLowLevel {
            attribute :>> statement = "units shall name every VT-d unit a DMAR
                describes and every SMMUv3 an IORT, or without ACPI a device tree,
                describes, each once and none at address 0 or of length 0, with the
                length of registers no driver may be given.";
            attribute :>> criterion = "The discovery line counts 1 VT-d unit and 0
                SMMUv3s on x86-64, and 0 and 1 on AArch64 and ARMv7-A, as the
                machines xtask boots configure them.";
            attribute :>> parent = ("H.DMA.1", "H.DMA.5");
            attribute :>> unit = ("iommu::units", "iommu::add", "iommu::unit_at");
        }
        requirement <'L.iommu.2'> placementsAreCounted : ItemLowLevel {
            attribute :>> statement = "report shall count on the console the PCI
                functions firmware places behind a unit, those
                it places behind none, and those it describes in a way the kernel
                cannot follow.";
            attribute :>> criterion = "On every machine xtask test-boot boots, the
                discovery line is printed and counts 1 or more PCI functions behind a
                unit and 0 unresolved.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::report", "iommu::discover");
        }
        requirement <'L.iommu.3'> dmarEndpointsArePlaced : ItemLowLevel {
            attribute :>> statement = "place_dmar shall place a function that a
                single-hop endpoint scope of a DRHD on its segment names behind that
                DRHD's unit, as its requester ID.";
            attribute :>> criterion = "On x86-64 each PCI function on bus 0 is
                reported behind the unit at the DRHD's register base as the stream
                bus << 8 | device << 3 | function.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::place_dmar", "iommu::behind", "iommu::discover");
        }
        requirement <'L.iommu.4'> iortMappingsArePlaced : ItemLowLevel {
            attribute :>> statement = "place_iort shall place a function behind the
                SMMUv3 that its segment's root complex maps its requester ID to, as
                the stream ID the mapping gives, and a function mapped to an ITS
                group behind no unit.";
            attribute :>> criterion = "On AArch64 each of the 5 PCI functions is
                reported behind the SMMUv3 at the IORT's base address as the stream
                its root complex's ID mapping gives.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::place_iort", "iommu::behind");
        }
        requirement <'L.iommu.5'> treeMapsArePlaced : ItemLowLevel {
            attribute :>> statement = "place_tree shall place a function behind the
                SMMUv3 its ECAM host's iommu-map names for its requester ID, as the
                stream ID the map gives, taking the host that names the function's
                segment or the only host.";
            attribute :>> criterion = "On ARMv7-A each of the 5 PCI functions is
                reported behind the SMMUv3 the device tree gives as the stream its
                host's iommu-map gives.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::place_tree", "iommu::behind");
        }
        requirement <'L.iommu.6'> whatCannotBeFollowedIsUnresolved : ItemLowLevel {
            attribute :>> statement = "A function firmware describes in a way the
                kernel cannot follow -- a DMAR scope whose path runs through a
                function that is not a bridge, or a function below a named bridge
                whose DMA arrives under a bridge's alias, an IORT mapping to a node that is not there or to an
                SMMUv1 or v2, a device tree whose hosts cannot be told apart, a map
                naming a unit that is not among the units -- shall be counted
                unresolved, never bypassing.";
            attribute :>> criterion = "Each of those descriptions, given for a
                function no endpoint scope or mapping otherwise places, is counted 1
                unresolved and 0 bypassing.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::place_dmar", "iommu::place_iort",
                "iommu::place_tree", "iommu::behind");
        }
    }

    package BringUpAndDomains {
        doc /* src/kernel/src/iommu.rs, bring-up and domain_for, and the halves of
             * iommu/vtd.rs and iommu/smmuv3.rs that program a unit and attach a
             * domain to it. */

        requirement <'L.iommu.7'> translationIsOn : ItemLowLevel {
            attribute :>> statement = "bring_up shall turn translation on for the
                VT-d units a DMAR describes and the SMMUv3s an IORT describes that it
                can drive.";
            attribute :>> criterion = "On x86-64 (VT-d) and AArch64 (SMMUv3) the
                entropy device's write outside its domain is refused and recorded by
                its unit, which a unit does only while translating: 1 or more
                out-of-domain writes faulted (the `pci` line).";
            attribute :>> parent = "H.DMA.2";
            attribute :>> unit = ("iommu::bring_up", "iommu::bring_up_vtd",
                "iommu::bring_up_smmu", "iommu::vtd::Unit::open",
                "iommu::vtd::Unit::enable", "iommu::vtd::Unit::command",
                "iommu::vtd::Unit::invalidate_context",
                "iommu::vtd::Unit::invalidate_iotlb", "iommu::vtd::Unit::wait",
                "iommu::vtd::table", "iommu::vtd::write64",
                "iommu::smmuv3::Unit::open", "iommu::smmuv3::Unit::program",
                "iommu::smmuv3::Unit::enable", "iommu::smmuv3::Unit::control",
                "iommu::smmuv3::Unit::command", "iommu::smmuv3::Unit::wait",
                "iommu::smmuv3::frames", "iommu::smmuv3::write64");
        }
        requirement <'L.iommu.8'> aUnitItCannotDriveIsLeftAlone : ItemLowLevel {
            attribute :>> statement = "bring_up shall leave alone, count and say why,
                a unit that is not what the driver needs -- a VT-d unit not of
                version 1, without three-level tables or needing its write buffer
                flushed, an SMMUv3 without an AArch64 stage 2, 8 stream ID bits or
                4 KiB tables with 40-bit output, either left translating by firmware
                -- or for which there is no frame, and shall unmap its registers.";
            attribute :>> criterion = "Each of those units is counted 1 left alone
                with its reason on the bring-up line, 0 translating, and its register
                window is unmapped.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::bring_up_vtd", "iommu::bring_up_smmu",
                "iommu::vtd::Unit::open", "iommu::smmuv3::Unit::open");
        }
        requirement <'L.iommu.9'> noDomainReachesNothing : ItemLowLevel {
            attribute :>> statement = "Once a unit translates, a function behind it
                with no domain shall reach nothing: VT-d finds no context entry, and
                every SMMUv3 stream table entry is valid and aborting until a domain
                is attached.";
            attribute :>> criterion = "A DMA by a function behind a translating unit
                that has been given no domain is refused and recorded against its
                stream.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::vtd::Unit::enable", "iommu::smmuv3::Unit::open",
                "iommu::smmuv3::write_entry");
        }
        requirement <'L.iommu.10'> aFunctionGetsADomainOnItsUnit : ItemLowLevel {
            attribute :>> statement = "domain_for shall give a function the DMAR puts
                behind a translating VT-d unit a domain on that unit as its source
                ID, and one an IORT root complex sends to a translating SMMUv3 a
                domain on it as the stream the mapping gives.";
            attribute :>> criterion = "On x86-64 and AArch64 the entropy device's
                domain names a stream, and its unit records the probe's refused write
                against that stream: 1 or more out-of-domain writes faulted (the
                `pci` line).";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::domain_for", "iommu::vtd_unit_for",
                "iommu::smmu_stream_for", "iommu::endpoints", "iommu::Domain::with",
                "iommu::Translation::stream", "iommu::vtd::Unit::attach",
                "iommu::vtd::Unit::install", "iommu::vtd::read_entry",
                "iommu::vtd::write_entry", "iommu::smmuv3::Unit::attach",
                "iommu::smmuv3::read_entry", "iommu::smmuv3::write_entry");
        }
        requirement <'L.iommu.11'> noUnitNoTranslation : ItemLowLevel {
            attribute :>> statement = "domain_for shall give an untranslated domain
                to a function no translating unit covers, including every function
                before bring_up and every function behind an SMMUv3 only a device
                tree describes.";
            attribute :>> criterion = "On ARMv7-A the first PCI function's domain is
                untranslated, and so is a domain made before bring_up.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::domain_for", "iommu::Domain::untranslated");
        }
        requirement <'L.iommu.12'> degradedModeIsAnnounced : ItemLowLevel {
            attribute :>> statement = "The first pin into an untranslated domain
                shall say on the console, once, that device DMA reaches all of
                memory: degraded trusted mode.";
            attribute :>> criterion = "An ARMv7-A boot prints the degraded trusted
                mode line exactly once, and an x86-64 or AArch64 boot, whose pins are
                translated, 0 times.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = "iommu::Domain::pin";
        }
        requirement <'L.iommu.13'> aSecondDomainIsRefused : ItemLowLevel {
            attribute :>> statement = "A unit shall refuse a second domain for a
                function or stream that has one, and domain_for shall then answer an
                untranslated domain and say why, never two domains translating one
                device.";
            attribute :>> criterion = "For the first PCI function, whose translated
                domain the stage 10 check pinned through, a second domain_for answers
                an untranslated domain.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::domain_for", "iommu::vtd::Unit::attach",
                "iommu::vtd::Unit::install", "iommu::smmuv3::Unit::attach");
        }
        requirement <'L.iommu.14'> aDroppedDomainGivesItsStreamBack : ItemLowLevel {
            attribute :>> statement = "A translated domain dropped with no page
                pinned shall detach from its unit, so that the function can be given
                a domain again.";
            attribute :>> criterion = "A translated domain made for a PCI function
                that had none, dropped with nothing pinned, is followed by a second
                domain_for for the function that is translated.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::Domain::drop", "iommu::Translation::detach",
                "iommu::vtd::Unit::detach", "iommu::smmuv3::Unit::detach",
                "iommu::smmuv3::Unit::release");
        }
        requirement <'L.iommu.15'> detachGivesBackWhatAttachTook : ItemLowLevel {
            attribute :>> statement = "detach shall return a domain's identifier or
                VMID and its root table, and on an SMMUv3 the doorbell's tables, only
                after the unit has forgotten the domain, and shall keep them when the
                unit never answers.";
            attribute :>> criterion = "Attaching and detaching a domain N times
                leaves the free-frame count and the identifiers in use as they were;
                a detach whose invalidation times out frees 0 frames.";
            attribute :>> parent = ("H.DMA.8", "H.MEM.9");
            attribute :>> unit = ("iommu::vtd::Unit::detach", "iommu::smmuv3::Unit::detach",
                "iommu::smmuv3::Unit::release");
        }
        requirement <'L.iommu.16'> aDomainInUseStaysAttached : ItemLowLevel {
            attribute :>> statement = "A translated domain dropped while pages are
                still pinned into it shall stay attached, its tables and context
                entry or stream table entry kept.";
            attribute :>> criterion = "A domain dropped holding a pin of one page
                still translates that page's address afterwards, and its function is
                refused a new domain.";
            attribute :>> parent = "H.DMA.8";
            attribute :>> unit = "iommu::Domain::drop";
        }
        requirement <'L.iommu.17'> aRefusedAttachLeavesNothing : ItemLowLevel {
            attribute :>> statement = "attach shall refuse a stream past the SMMUv3's
                table, a unit with no identifier or VMID left, and a function or
                stream it cannot record for want of memory or a frame, and a refused
                attach shall give back the root table it took and write no entry.";
            attribute :>> criterion = "Each refusal leaves the free-frame count, the
                identifiers in use and the context or stream table entry as they
                were.";
            attribute :>> parent = ("H.DMA.3", "H.MEM.11");
            attribute :>> unit = ("iommu::vtd::Unit::attach", "iommu::vtd::Unit::install",
                "iommu::smmuv3::Unit::attach");
        }
        requirement <'L.iommu.18'> everyDomainMapsTheDoorbell : ItemLowLevel {
            attribute :>> statement = "Every SMMUv3 domain shall map the GICv2m MSI
                doorbell page at its own address, so that a device's MSI writes,
                which the unit translates like any DMA, reach the interrupt
                controller.";
            attribute :>> criterion = "A device in a translated SMMUv3 domain that
                completes a request by MSI-X has the completion delivered, and a
                domain without the doorbell mapped has its MSI write faulted.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("iommu::smmuv3::Unit::attach",
                "iommu::smmuv3::Unit::detach");
        }
    }

    package Pins {
        doc /* src/kernel/src/iommu.rs, Domain::pin and Domain::unpin: the domain
             * side of every pin, the same for both kinds of unit, and the
             * unit's flush that ends an unpin. */

        requirement <'L.iommu.19'> aPinAddressesEachFrame : ItemLowLevel {
            attribute :>> statement = "pin shall map each frame it is given at its
                own physical address, answer each page's device address in the order
                the frames were given, and count the pages it holds.";
            attribute :>> criterion = "A pin of two frames through the first PCI
                function's domain answers the frames' physical addresses in order,
                the domain resolves each to itself, and its pinned-page count rises
                by 2.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::Domain::pin", "iommu::map_all",
                "iommu::Translation::changing", "iommu::Translation::map",
                "iommu::Translation::flush", "iommu::Translation::resolve",
                "iommu::Pinned::addresses");
        }
        requirement <'L.iommu.20'> anUnpinTakesThePagesOut : ItemLowLevel {
            attribute :>> statement = "unpin shall take a pin's pages out of its
                domain's tables and count them back.";
            attribute :>> criterion = "After its own domain unpins the two-frame pin,
                the pinned-page count is back where it was and, on a translated
                domain, neither frame's address resolves.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::Domain::unpin", "iommu::Translation::unmap",
                "iommu::Translation::resolve");
        }
        requirement <'L.iommu.21'> onlyItsOwnDomainUnpins : ItemLowLevel {
            attribute :>> statement = "unpin shall refuse a pin another domain took
                with Foreign, and hand the pin back unchanged.";
            attribute :>> criterion = "A domain asked to unpin the two-frame pin of
                another domain refuses Foreign and hands the pin back, which its own
                domain then unpins.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "iommu::Domain::unpin";
        }
        requirement <'L.iommu.22'> anEmptyPinIsRefused : ItemLowLevel {
            attribute :>> statement = "pin shall refuse a pin of no frames with
                Empty.";
            attribute :>> criterion = "A pin of an empty list of frames is refused
                Empty.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "iommu::Domain::pin";
        }
        requirement <'L.iommu.23'> aFramePastTheTablesIsRefused : ItemLowLevel {
            attribute :>> statement = "pin shall refuse with OutOfRange, mapping
                nothing, a frame whose address does not fit in 64 bits or, in a
                translated domain, in the 39 bits its tables reach.";
            attribute :>> criterion = "A pin into a translated domain naming one
                frame at or above 2^39 bytes, beside one below, is refused OutOfRange
                with the pinned-page count unchanged and neither address resolving.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "iommu::Domain::pin";
        }
        requirement <'L.iommu.24'> aPageIsPinnedOnce : ItemLowLevel {
            attribute :>> statement = "A pin into a translated domain of a page
                already pinned there shall be refused with AlreadyPinned.";
            attribute :>> criterion = "On a translated domain, a vmo_pin of a page
                already pinned into it is refused ALREADY_BOUND.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("iommu::map_all", "iommu::Domain::pin");
        }
        requirement <'L.iommu.25'> aFailedPinUndoesItself : ItemLowLevel {
            attribute :>> statement = "A pin refused part-way shall unmap every page
                it had mapped, and give back the tables that emptied once the unit
                has forgotten them.";
            attribute :>> criterion = "A pin of three frames whose third is already
                pinned is refused, and afterwards the first two do not resolve and
                the free-frame count is as it was.";
            attribute :>> parent = ("H.DMA.3", "H.MEM.11");
            attribute :>> unit = "iommu::map_all";
        }
        requirement <'L.iommu.26'> theUnitForgetsBeforeUnpinReturns : ItemLowLevel {
            attribute :>> statement = "unpin shall return only once the unit has
                completed the invalidation of the pages it took out, and shall
                answer Unit, handing the pin back, when the unit never completes
                it.";
            attribute :>> criterion = "An unpin from a unit made never to finish its
                invalidation answers Unit and hands the pin back; from a unit that
                finishes, it returns after the unit's completion was read.";
            attribute :>> parent = "H.DMA.8";
            attribute :>> unit = ("iommu::Domain::unpin", "iommu::Translation::flush",
                "iommu::vtd::Unit::flush", "iommu::vtd::Unit::invalidate_iotlb",
                "iommu::smmuv3::Unit::flush", "iommu::smmuv3::Unit::command");
        }
        requirement <'L.iommu.27'> emptiedTablesWaitForTheFlush : ItemLowLevel {
            attribute :>> statement = "A table an unpin or an undone pin empties
                shall go back to the frame allocator only after the unit's
                invalidation has completed, and shall be kept when it has not.";
            attribute :>> criterion = "An unpin that empties a table frees it only
                after the flush answers; with the flush made to fail, the table stays
                out of the allocator.";
            attribute :>> parent = "H.DMA.8";
            attribute :>> unit = ("iommu::Domain::unpin", "iommu::map_all");
        }
        requirement <'L.iommu.28'> aMapInCachingModeIsFlushed : ItemLowLevel {
            attribute :>> statement = "A VT-d unit in caching mode shall be told to
                forget a domain's cached translations after a map as well as after an
                unmap, since it caches entries that were not present.";
            attribute :>> criterion = "Under a VT-d unit in caching mode, a device
                reaches a page pinned after the same address had been walked while
                unmapped, with 0 faults recorded.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = "iommu::vtd::Unit::flush";
        }
        requirement <'L.iommu.29'> anUnpinWaitsWithInterruptsOn : ItemLowLevel {
            attribute :>> statement = "A wait on a unit made in a context that may
                block shall be made with interrupts on, and counted.";
            attribute :>> criterion = "On a translated domain, the boot task's unpin
                of the two-frame pin raises the count of unit waits made in a context
                that may block.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("iommu::gate::poll", "iommu::vtd::Unit::wait",
                "iommu::smmuv3::Unit::wait");
        }
    }

    package Gate {
        doc /* src/kernel/src/iommu/gate.rs: how an operation waits for a unit
             * without masking interrupts. */

        requirement <'L.iommu.30'> aWaitEndsAtItsAnswerOrDeadline : ItemLowLevel {
            attribute :>> statement = "poll shall answer true once the unit answers
                before the deadline, and false after one last look past it.";
            attribute :>> criterion = "A wait on a unit that never answers, with its
                deadline already past, answers false; a wait by a task that may
                block, on a unit that answers on its 101st look, with a deadline 5 s
                away, answers true.";
            attribute :>> parent = "H.DMA.8";
            attribute :>> unit = ("iommu::gate::poll", "iommu::gate::look");
        }
        requirement <'L.iommu.31'> oneOperationAtATime : ItemLowLevel {
            attribute :>> statement = "A gate shall admit one operation at a time,
                refuse the next once its patience has passed, and admit the next once
                the holder leaves.";
            attribute :>> criterion = "With a gate held, an entry with a patience of
                10 ms is refused; once the holder leaves, an entry is admitted.";
            attribute :>> parent = "H.DMA.8";
            attribute :>> unit = ("iommu::gate::Gate::enter_within",
                "iommu::gate::Gate::enter", "iommu::gate::Entered::drop");
        }
        requirement <'L.iommu.32'> aWaiterIsWokenWhenTheGateIsLeft : ItemLowLevel {
            attribute :>> statement = "A task waiting to enter a held gate shall
                sleep, and shall be woken and admitted when the holder leaves.";
            attribute :>> criterion = "A second task waiting on a gate the first
                holds for 10 ms is admitted after the first leaves and before its own
                patience has passed, having slept meanwhile.";
            attribute :>> parent = "H.SCHED.1";
            attribute :>> unit = ("iommu::gate::Gate::enter_within",
                "iommu::gate::Entered::drop");
        }
        requirement <'L.iommu.33'> aUnitThatNeverAnswersIsGivenUpOn : ItemLowLevel {
            attribute :>> statement = "A unit's commands shall run one at a time,
                each waited for up to the unit's patience, and one the unit does not
                finish, or an SMMUv3 command it stops on, shall answer an error.";
            attribute :>> criterion = "A command to a unit made never to finish
                answers an error within its patience, and an SMMUv3 command queue
                stopped with an error answers it refused.";
            attribute :>> parent = "H.DMA.8";
            attribute :>> unit = ("iommu::vtd::Unit::command",
                "iommu::vtd::Unit::invalidate_context",
                "iommu::vtd::Unit::invalidate_iotlb", "iommu::smmuv3::Unit::command",
                "iommu::smmuv3::Unit::control");
        }
    }

    package Faults {
        doc /* src/kernel/src/iommu.rs, the faults half, and the fault readers of
             * iommu/vtd.rs and iommu/smmuv3.rs. */

        requirement <'L.iommu.34'> faultsReadAsRecorded : ItemLowLevel {
            attribute :>> statement = "A fault shall print as its unit recorded it: a
                refused access by its stream, page and direction, an SMMUv3 event by
                its type's name, an event queue's overflow and a VT-d unit's lost
                fault as such.";
            attribute :>> criterion = "A refused read and a refused write, a lost
                SMMUv3 event, a VT-d overflow and SMMUv3 events 0x10 and 0xFF each
                print the words given for them, 0xFF as reserved.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = ("iommu::Fault::fmt", "iommu::smmuv3::event_name");
        }
        requirement <'L.iommu.35'> aVtdFaultIsReadWhole : ItemLowLevel {
            attribute :>> statement = "A VT-d unit's take_fault shall answer the
                refused access its first record holds as the source ID, page and
                direction the unit wrote.";
            attribute :>> criterion = "On x86-64 the probe reads its fault back as a
                write by the entropy device's stream to the probe's page: 1 or more
                out-of-domain writes faulted (the `pci` line).";
            attribute :>> parent = "H.DMA.2";
            attribute :>> unit = ("iommu::vtd::Unit::take_fault",
                "iommu::Translation::take_fault", "iommu::Domain::take_fault");
        }
        requirement <'L.iommu.36'> anSmmuFaultIsRead : ItemLowLevel {
            attribute :>> statement = "An SMMUv3's take_fault shall answer a
                translation, address size, access flag or permission event as a
                refused access by its stream, page and direction.";
            attribute :>> criterion = "On AArch64 the probe reads its fault back as a
                write by the entropy device's stream to the probe's page: 1 or more
                out-of-domain writes faulted (the `pci` line).";
            attribute :>> parent = "H.DMA.2";
            attribute :>> unit = ("iommu::smmuv3::Unit::take_fault",
                "iommu::smmuv3::read_entry", "iommu::Translation::take_fault",
                "iommu::Domain::take_fault");
        }
        requirement <'L.iommu.37'> aLostVtdFaultIsReported : ItemLowLevel {
            attribute :>> statement = "A VT-d unit's take_fault shall clear each
                record it answers, and answer a primary fault overflow, once the
                record is empty, as an Overflow against the record taken last, and
                clear it.";
            attribute :>> criterion = "With the record full and a second fault
                dropped, take_fault answers the full record's access, then 1
                Overflow naming it, then nothing.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = "iommu::vtd::Unit::take_fault";
        }
        requirement <'L.iommu.38'> everySmmuEventIsReported : ItemLowLevel {
            attribute :>> statement = "An SMMUv3's take_fault shall consume each
                event it answers, answer every event type but the four refused
                accesses as an Event naming it, and answer an event queue that
                overflowed as Lost, first and acknowledged.";
            attribute :>> criterion = "A queue holding a C_BAD_STREAMID event
                answers Event(0x02); a queue whose overflow flag differs from the
                acknowledgement answers Lost once and then its events, each once.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = "iommu::smmuv3::Unit::take_fault";
        }
        requirement <'L.iommu.39'> firmwaresFaultIsNotOurs : ItemLowLevel {
            attribute :>> statement = "enable shall clear a fault firmware left
                recorded in a VT-d unit, and its overflow, before translation goes
                on.";
            attribute :>> criterion = "A unit brought up with a fault and an overflow
                left recorded answers no fault from take_fault afterwards.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = "iommu::vtd::Unit::enable";
        }
        requirement <'L.iommu.40'> aProvokedFaultIsNamedFirst : ItemLowLevel {
            attribute :>> statement = "provoke shall clear the unit's faults and
                register the device's stream and the page, saying so on the console,
                before the device is made to write it.";
            attribute :>> criterion = "After provoke, the unit holds no fault, and
                the stream and page are registered and printed on the `on purpose`
                line.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = ("iommu::Domain::provoke", "iommu::Domain::clear_faults",
                "iommu::record_provoked");
        }
        requirement <'L.iommu.41'> aProvokedFaultIsNotStray : ItemLowLevel {
            attribute :>> statement = "A refused access by a registered stream to
                its registered page, and a VT-d overflow whose full record held one,
                shall not be counted stray, however late it is read.";
            attribute :>> criterion = "A fault equal to a registered stream and page,
                taken through a domain and again by the audit, leaves the stray count
                unchanged; the same fault at another page raises it by 1.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = ("iommu::provoked", "iommu::count_if_stray");
        }
        requirement <'L.iommu.42'> aStrayFaultIsCounted : ItemLowLevel {
            attribute :>> statement = "A fault taken that no check provoked shall be
                counted stray, and apart when it is not a refused access, whether a
                check, the clearing before a probe or the audit took it.";
            attribute :>> criterion = "An SMMUv3 event and a VT-d overflow no check
                registered each raise the stray count and the stray-event count by
                1.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = ("iommu::count_if_stray", "iommu::Domain::take_fault",
                "iommu::Domain::clear_faults");
        }
        requirement <'L.iommu.43'> theAuditReadsEveryUnit : ItemLowLevel {
            attribute :>> statement = "audit_faults shall take the faults every
                translating unit holds, up to 256 a unit, and report the faults no
                check provoked since translation went on, the first it read itself,
                and the provoked ones it found late.";
            attribute :>> criterion = "With 1 stray fault left in a unit's record,
                the audit reports 1 unit read, 1 stray and that fault as the first.";
            attribute :>> parent = "H.DMA.6";
            attribute :>> unit = ("iommu::audit_faults", "iommu::drain",
                "iommu::recorded");
        }
        requirement <'L.iommu.44'> aRefusedAccessIsAuditRecorded : ItemLowLevel {
            attribute :>> statement = "Every fault read from a unit, by the fault
                audit or through a domain, shall be written to the audit record as
                one DMA_FAULT, refused, naming the device's stream and the page it
                addressed, with the kernel as its subject.";
            attribute :>> criterion = "On a boot where a unit translates, the audit
                record holds a DMA_FAULT from the out-of-domain probe, refused, with
                pid 0; with the record's event swapped at recorded, or its subject
                made a process, the end-of-boot audit check fails naming it.";
            attribute :>> parent = "H.DMA.2";
            attribute :>> unit = ("iommu::recorded", "audit::record",
                "audit::Store::record_at");
        }
        requirement <'L.iommu.45'> dmarBridgesAreFollowed : ItemLowLevel {
            attribute :>> statement = "place_dmar shall follow a DMAR device
                scope's path through the bridges enumeration found, and shall place
                a sub-hierarchy scope's bridge, and every function below it whose
                path up to the bridge crosses only PCIe ports, behind the DRHD's
                unit as the function's own requester ID.";
            attribute :>> criterion = "Host tests: a path is followed through each
                bridge's secondary bus; a function behind a root port or a switch
                is its own requester; one behind a PCIe-to-PCI bridge, on a bus no
                bridge explains, or under a loop of bus numbers is aliased, never
                its own. On x86-64 the virtio-rng function behind a PCIe root port
                is placed behind the unit, 0 functions are unresolved, and its
                out-of-domain write is faulted.";
            attribute :>> parent = "H.DMA.1";
            attribute :>> unit = ("iommu::place_dmar", "iommu::learn_bridge",
                "iommu::Scope::places", "iommu::vtd_unit_for");
        }
    }
}
