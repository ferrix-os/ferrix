/*
 * Ferrix — SysML v2 model, part 20: the low-level requirements of a
 * device's claim, its node, and its quiesce.
 *
 * What each unit of src/kernel/src/claim.rs and src/kernel/src/device.rs does, and
 * the quiesce in src/kernel/src/syscall/native.rs, as `ItemLowLevel`
 * requirements (part 13 defines the format): a device claimed through its
 * core's control channels, the number its node is published under, the
 * apertures and vectors a node hands out and nothing past them, the MSI-X
 * vectors it mints, the bus mastering a quiesce turns off, and the quiesce
 * itself, which waits out every core that serves the device before the next
 * driver is given it. The objects a driver holds for a device -- its
 * mapping, its interrupt, its pins -- are object/'s (part 14); the domain
 * under them is iommu's (part 16).
 *
 * The format and its rules are the object/ pilot's (W-8 step 3): a
 * requirement names every function that carries its behaviour as its
 * `unit`; one check proves a whole requirement; a statement says no more
 * than its criterion tests; an accessor needs no requirement of its own.
 *
 * What proves what. The claims and numbers are proved by service_check.rs,
 * on claims of its own. A node's rules are proved on every boot, before the
 * nodes are published, by device/check.rs, which `device::publish` runs.
 * The quiesce is proved by block_ring/check.rs, which drives
 * `device_quiesce` through a driver's life and death on the first PCI
 * function, and by xtask's `test-init`, which restarts `devmgr`. What no
 * check proves waits in the baseline as check-writing work.
 *
 * Ids are `L.claim.<n>`, `L.device.<n>` and `L.quiesce.<n>`, numbered
 * through each part; `L.quiesce` names the behaviour, whose units are in
 * syscall/native.rs.
 */
package FerrixDeviceRequirements {
    private import FerrixItemRequirements::*;

    package Claims {
        doc /* src/kernel/src/claim.rs: a device claimed through a core's control
             * channel, which a quiesce waits out, and the numbers the cores'
             * nodes are published under. */

        requirement <'L.claim.1'> aNodeIsClaimedOnce : ItemLowLevel {
            attribute :>> statement = "Claims::claim shall record a claim of a
                node nobody holds, and refuse a second claim of it while the first
                stands.";
            attribute :>> criterion = "A published node nobody holds is claimed,
                and a second claim of it is refused.";
            attribute :>> parent = "H.DEV.1";
            attribute :>> unit = ("claim::Claims::claim", "claim::Claims::claim_up_to");
        }
        requirement <'L.claim.2'> aSharedNodeUpToItsLimit : ItemLowLevel {
            attribute :>> statement = "Claims::claim_up_to shall record up to its
                limit of claims of one node, each through its own channel, and
                refuse one more.";
            attribute :>> criterion = "At a limit of 2, two claims of one node
                through two channels are recorded and a third is refused.";
            attribute :>> parent = "H.DEV.1";
            attribute :>> unit = "claim::Claims::claim_up_to";
        }
        requirement <'L.claim.3'> refusedUnderAnyLiveDriver : ItemLowLevel {
            attribute :>> statement = "Claims::wait_until_released shall answer
                ByADriver, at once, while any driver end a node is claimed
                through is still open.";
            attribute :>> criterion = "With a node claimed through two channels,
                one driver end closed and the other open, the wait answers
                ByADriver.";
            attribute :>> parent = "H.DEV.2";
            attribute :>> unit = ("claim::Claims::wait_until_released",
                "claim::Claims::wait_within", "claim::Claims::is_held_by_a_driver");
        }
        requirement <'L.claim.4'> aReleaseEndsTheWait : ItemLowLevel {
            attribute :>> statement = "A wait for a node whose drivers have gone
                shall answer Ok once no claim of it stands, whether its release
                came before the wait began or while the wait was parked.";
            attribute :>> criterion = "A wait begun after the node's release ends
                Ok at once, and a wait parked on a node released by another task
                once the wait is blocked ends Ok.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = ("claim::Claims::wait_within",
                "claim::Claims::is_claimed", "claim::Claims::release");
        }
        requirement <'L.claim.5'> aCancelledWaitIsToldWaiting : ItemLowLevel {
            attribute :>> statement = "A wait for a node still claimed shall answer
                Waiting when its caller is cancelled.";
            attribute :>> criterion = "A wait on a node whose driver end has closed
                and whose claim stands, cancelled, answers Waiting; after the
                release, the same wait answers Ok.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = "claim::Claims::wait_within";
        }
        requirement <'L.claim.6'> theWaitHasAPatience : ItemLowLevel {
            attribute :>> statement = "A wait for a node still claimed shall answer
                Waiting when its patience runs out.";
            attribute :>> criterion = "A wait of 10 ms on a node whose driver end
                has closed and whose claim stands answers Waiting.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = "claim::Claims::wait_within";
        }
        requirement <'L.claim.7'> oneClaimLetGo : ItemLowLevel {
            attribute :>> statement = "Claims::release_one shall let go only the
                claim made through its channel, leaving the node's others
                standing.";
            attribute :>> criterion = "Of a node's two claims, one let go leaves
                the node waited for under the other's driver; the other let go
                frees it.";
            attribute :>> parent = "H.DEV.1";
            attribute :>> unit = "claim::Claims::release_one";
        }
        requirement <'L.claim.8'> refusedForMemory : ItemLowLevel {
            attribute :>> statement = "A claim, or a node's number, with no memory
                to record it shall be refused and record nothing.";
            attribute :>> criterion = "With the one allocation of a claim refused,
                the claim is refused and the next, with memory, is recorded; with
                the one allocation of a number refused, none is taken and the next
                is the first.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("claim::Claims::claim_up_to", "claim::Numbers::take");
        }
        requirement <'L.claim.9'> theLowestFreeNumber : ItemLowLevel {
            attribute :>> statement = "Numbers::take shall give the lowest number
                from the first nothing holds, and a number given back shall be the
                next taken.";
            attribute :>> criterion = "From 7, two numbers taken are 7 and 8; 7
                given back is the next taken.";
            attribute :>> parent = "H.DEV.3";
            attribute :>> unit = ("claim::Numbers::take", "claim::Numbers::give_back");
        }
    }

    package Nodes {
        doc /* src/kernel/src/device.rs: the apertures and vectors a node hands
             * out, the nodes published, and the bus mastering a node's DMA
             * is switched with. */

        requirement <'L.device.1'> aperturesClearOfTheKernel : ItemLowLevel {
            attribute :>> statement = "DeviceNode::mint shall withhold, and count,
                an aperture that overlaps memory the kernel uses, so that no
                published aperture does -- every aperture but the firmware
                framebuffer node's, whose one exemption, inside the boot
                framebuffer alone, is L.device.38. PCI enumeration shall turn on
                memory decoding that firmware left off only for an endpoint every
                memory BAR of which is assigned, clear of every reserved range
                (RAM among them) and of every other function's assigned memory BAR
                on its host bridge, and otherwise leave it off with a line naming
                why.";
            attribute :>> criterion = "Every aperture of every published node is
                clear of every reserved range, the firmware framebuffer node's
                clear of every one but the framebuffer. On made-up BARs,
                decoding_refusal allows assigned BARs clear of everything and
                refuses a BAR unassigned, one over a reserved range and one over
                another function's BAR; with the reserved test removed, stage 10
                fails.";
            attribute :>> parent = ("H.MEM.10", "H.DMA.5");
            attribute :>> unit = ("device::DeviceNode::mint", "device::Reserved::covers",
                "device::Aperture::overlaps", "discovery::pci::decoding_refusal",
                "discovery::pci::turn_decoding_on", "discovery::pci::decoding_after",
                "discovery::pci::memory_spans", "discovery::pci::examine_all");
        }
        requirement <'L.device.2'> anApertureGrantsExactlyItsRange : ItemLowLevel {
            attribute :>> statement = "DeviceNode::aperture shall grant a range
                only when it lies inside one of the node's apertures, and none of
                length 0 or past the address space's end.";
            attribute :>> criterion = "For every published aperture: the whole is
                granted, and its last byte; a range one byte past its end, one
                ending before its start by a byte, one of length 0 and one wrapping
                the address space are refused.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = ("device::DeviceNode::aperture", "device::Aperture::end");
        }
        requirement <'L.device.3'> interruptTablesNeverGranted : ItemLowLevel {
            attribute :>> statement = "DeviceNode::pci shall keep a function's
                MSI-X table and pending-bit pages out of every aperture it
                grants, unless the function's MSI-X is held off (L.device.35).";
            attribute :>> criterion = "For every published PCI function whose MSI-X
                is not held off, the whole of each MSI-X table or pending-bit
                range, its first byte and its last byte are refused.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = "device::DeviceNode::pci";
        }
        requirement <'L.device.4'> staticVectorsAsRecorded : ItemLowLevel {
            attribute :>> statement = "DeviceNode::vector shall hand out a node's
                recorded vectors as recorded, and none past its count.";
            attribute :>> criterion = "For every published node without MSI-X,
                each index below the count answers its recorded vector and the
                count itself answers none.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = "device::DeviceNode::vector";
        }
        requirement <'L.device.5'> nodesShareNothing : ItemLowLevel {
            attribute :>> statement = "publish shall take from a later node an
                aperture an earlier one holds, so that no two published nodes'
                apertures overlap; widen a PCI BAR aperture shorter than a page and
                starting on one to that page only when nothing else lies in the
                page -- no aperture of any node, published or dropped, no assigned
                memory BAR of any PCI function, decoded, undecoded or withheld, no
                withheld MSI-X table or pending-bit array span, and no reserved
                range; and publish no device tree vector that is not a shared
                peripheral interrupt or that two nodes hold.";
            attribute :>> criterion = "Sorted by start, no two published apertures
                overlap; every device tree vector is 32 or above and held by one
                node. On made-up nodes a 2 KiB BAR alone in its page is widened to
                the page, and one whose page also holds another node's aperture,
                another function's withheld MSI-X span, an undecoded function's
                assigned BAR or a reserved range is not; with the interrupt tables
                left out of the page test, stage 10 fails.";
            attribute :>> parent = ("H.DMA.5", "H.IRQ.1");
            attribute :>> unit = ("device::publish", "device::occupants",
                "device::page_neighbour", "device::widen_lone_partial_bars");
        }
        requirement <'L.device.6'> msixMintedOnce : ItemLowLevel {
            attribute :>> statement = "MsixTable::mint shall mint an entry once and
                answer the same vector when asked again, never on a number that
                has a handler, and none past the table.";
            attribute :>> criterion = "On the first published MSI-X table, entry 0
                minted twice is one vector, its number has no handler, and the
                entry at the table's size is refused.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("device::MsixTable::mint", "device::MsixTable::open");
        }
        requirement <'L.device.7'> msixMaskedAsTold : ItemLowLevel {
            attribute :>> statement = "A minted MSI-X entry shall be masked until
                unmasked, and shall read back masked or unmasked as it was last
                told.";
            attribute :>> criterion = "The first published MSI-X table's minted
                entry 0 reads masked; unmasked it reads unmasked, masked again it
                reads masked.";
            attribute :>> parent = ("H.IRQ.2", "H.IRQ.3");
            attribute :>> unit = ("device::MsixTable::set_masked",
                "device::MsixTable::is_masked", "device::Vector::set_masked");
        }
        requirement <'L.device.8'> aDomainPerNode : ItemLowLevel {
            attribute :>> statement = "DeviceNode::domain shall make a node's domain
                once and answer that one domain afterwards.";
            attribute :>> criterion = "Every published node asked for its domain
                twice answers one domain.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "device::DeviceNode::domain";
        }
        requirement <'L.device.9'> dmaIsSwitchedAtTheFunction : ItemLowLevel {
            attribute :>> statement = "enable_dma shall set a PCI function's bus
                mastering and memory decoding, and disable_dma shall clear its bus
                mastering and keep its memory decoding.";
            attribute :>> criterion = "On the first published function with a
                configuration space that is not a bridge (class 06), its command
                register reads bus mastering and decoding on after enable_dma, and
                bus mastering off with decoding on after disable_dma.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = ("device::DeviceNode::enable_dma",
                "device::DeviceNode::disable_dma", "device::DeviceNode::set_bus_master");
        }
        requirement <'L.device.10'> aPciNodeDescribedAsItIs : ItemLowLevel {
            attribute :>> statement = "DeviceNode::describe shall report a PCI
                function's location, identity, aperture and vector counts and virtio
                placement as the node holds them.";
            attribute :>> criterion = "For the first published PCI function,
                device_info's location, vendor and device, counts and virtio
                blocks agree with the node and with enumeration.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = ("device::DeviceNode::describe", "device::VirtioBlocks::of");
        }
        requirement <'L.device.11'> aFailureNamesItsNode : ItemLowLevel {
            attribute :>> statement = "A node's failure shall read as its location
                -- a PCI function's address, a device tree node's registers --
                and what it did.";
            attribute :>> criterion = "A failure of a PCI function, a virtio,mmio
                node and a device tree node reads as the address or registers and
                the sentence.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("device::Location::fmt", "device::Failure::fmt");
        }
        requirement <'L.device.12'> theLogOnlyToItsReader : ItemLowLevel {
            attribute :>> statement = "DeviceNode::reads_log shall allow the kernel
                log to no node but a GS201's DWC3.";
            attribute :>> criterion = "A published node of another binding asking
                for the kernel log is refused.";
            attribute :>> parent = "H.OBJ.2";
            attribute :>> unit = "device::DeviceNode::reads_log";
        }
        requirement <'L.device.13'> noClockWithoutABoard : ItemLowLevel {
            attribute :>> statement = "board_clock shall answer no clock for a node
                whose board keeps none for it.";
            attribute :>> criterion = "A clock asked of a published node whose
                board keeps none is refused.";
            attribute :>> parent = "H.OBJ.2";
            attribute :>> unit = ("discovery::board::board_clock", "device::DeviceNode::tree_binding");
        }
        requirement <'L.device.14'> reservedIsWhatTheKernelUses : ItemLowLevel {
            attribute :>> statement = "Reserved::of shall reserve the framebuffer,
                every memory-map region but MMIO, the ECAM windows, the kernel's
                device windows, every IOMMU unit's registers and, without ACPI, the
                device tree console.";
            attribute :>> criterion = "Each of those ranges on the booted machine
                is covered by the reserved set.";
            attribute :>> parent = ("H.MEM.10", "H.DMA.5");
            attribute :>> unit = ("device::Reserved::of", "device::Reserved::add",
                "device::Reserved::overlaps");
        }
        requirement <'L.device.15'> aHostVisibleWindow : ItemLowLevel {
            attribute :>> statement = "A function's host-visible window shall be
                taken only from an assigned memory BAR it fits, in whole pages,
                clear of reserved memory.";
            attribute :>> criterion = "A window from a BAR that is not assigned,
                does not fit, is not whole pages or overlaps reserved memory is
                refused, and one that is all four is taken.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = ("device::Window::of", "device::DeviceNode::host_visible");
        }
        requirement <'L.device.16'> aHostTableOnlyWhereItIs : ItemLowLevel {
            attribute :>> statement = "MsixTable::of shall find a function's MSI-X
                table only inside an assigned memory BAR, clear of reserved
                memory.";
            attribute :>> criterion = "A table in an unassigned BAR, past its BAR or
                overlapping reserved memory is not found.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = "device::MsixTable::of";
        }
        requirement <'L.device.17'> legacyLinesOnceEach : ItemLowLevel {
            attribute :>> statement = "A PCI function's legacy interrupt shall be
                one controller vector with its firmware trigger, on a line of 32 or
                above that no kernel handler holds and no other function took.";
            attribute :>> criterion = "Two functions offered one line give it to the
                first; a line below 32 or with a handler is given to neither.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("device::claim_line", "device::trigger_of");
        }
        requirement <'L.device.18'> treeAndBoardNodes : ItemLowLevel {
            attribute :>> statement = "The device tree's and the boards' finders
                shall publish a device tree node only with registers clear of
                reserved memory and an interrupt no other node or handler holds,
                and leave it alone, saying why, otherwise.";
            attribute :>> criterion = "A board node whose registers overlap reserved
                memory, or whose interrupt is taken, is left alone and named on the
                console; one clear of both is published.";
            attribute :>> parent = ("H.DMA.5", "H.IRQ.1");
            attribute :>> unit = ("discovery::tree::VirtioMmio::find",
                "discovery::tree::add_lines", "discovery::board::Boards::find",
                "discovery::board::board_node",
                "device::DeviceNode::add_line", "device::DeviceNode::bind_board");
        }
        requirement <'L.device.19'> aTreeNodeDescribed : ItemLowLevel {
            attribute :>> statement = "DeviceNode::describe shall report a device
                tree node as not PCI, with its binding and its first two
                apertures.";
            attribute :>> criterion = "A published virtio,mmio or board node's
                device_info names no PCI location, its binding, and its apertures
                0 and 1.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = "device::DeviceNode::describe";
        }
        requirement <'L.device.20'> nothingMintedUnpublished : ItemLowLevel {
            attribute :>> statement = "An MSI-X vector shall be minted, masked or
                unmasked only for a published node.";
            attribute :>> criterion = "A node not in the published list asked for
                an MSI-X vector answers none, and a vector naming one refuses to
                mask.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = ("device::DeviceNode::vector", "device::Vector::set_masked");
        }
        requirement <'L.device.21'> inputFunctionsOfAHost : ItemLowLevel {
            attribute :>> statement = "DeviceNode::input_functions shall allow
                eight input claims of a USB host's node -- an STM32 USB host's,
                or a PCI function whose class code is exactly xHCI's, 0C:03:30
                -- and one of every other.";
            attribute :>> criterion = "An STM32 USB host's node and a PCI
                function of class 0C:03:30 answer 8; a PCI function of class
                0C:03:20 and a virtio PCI function answer 1.";
            attribute :>> parent = "H.DEV.1";
            attribute :>> unit = "device::DeviceNode::input_functions";
        }
        requirement <'L.device.22'> msiMintedMaskedAndDelivered : ItemLowLevel {
            attribute :>> statement = "A PCI function with an MSI capability and no
                MSI-X table the kernel uses shall be offered one vector, minted once with INTx off --
                written under the node's command-register lock, which bus mastering's
                writes take too (L.object.117) -- and the message masked, none past the one
                message, and no vector allocated for a 32-bit capability the
                architecture's message address is above.";
            attribute :>> criterion = "On x86-64 test-boot requires the devices
                line's 1 vectors minted and 2 deliveries: QEMU's edu device's vector
                0 minted twice is one vector with no handler on its number, vector 1
                is refused, it reads masked as minted and as told, the command
                register reads INTx off, and edu's raised interrupt arrives twice
                unmasked and not while masked. Without edu on the machine, or with
                the INTx write skipped, the boot fails.";
            attribute :>> parent = ("H.IRQ.1", "H.IRQ.2", "H.IRQ.3");
            attribute :>> unit = ("device::MsiFunction::mint",
                "device::MsiFunction::of", "device::DeviceNode::mapped_config",
                "device::MsiFunction::intx_off", "device::DeviceNode::vector",
                "device::DeviceNode::vector_count", "device::DeviceNode::set_bus_master");
        }
        requirement <'L.device.23'> msiRegistersByLayout : ItemLowLevel {
            attribute :>> statement = "An MSI message shall be programmed with MSI
                off while its registers are written, at the offsets its
                capability's address width puts them, and left masked: by its mask
                bit, with MSI on, where the function can mask its vector, and by MSI
                Enable otherwise; masking, unmasking and reading back use the same
                bit.";
            attribute :>> criterion = "Host tests: a maskable 32-bit capability is
                programmed with its data at +8, its mask bit set and MSI on, and is
                masked and unmasked by the mask bit with Enable untouched; a
                non-maskable 64-bit one gets both address halves, its data at +0xC
                and MSI off, and is masked and unmasked by Enable; one message is
                enabled whatever the function could use. With the two paths swapped
                the tests fail.";
            attribute :>> parent = ("H.IRQ.2", "H.IRQ.3");
            attribute :>> unit = ("device::MsiFunction::set_masked",
                "device::MsiFunction::is_masked", "device::MsiFunction::mint");
        }
        requirement <'L.device.24'> aperturesReportedWhole : ItemLowLevel {
            attribute :>> statement = "device_aperture shall report each of a
                device's apertures with the address, length, BAR, offset in that BAR
                and flags it was minted with, whole however far past 4 GiB, and
                refuse an index past them with INVALID_ARGS; DeviceInfo shall keep
                its 96 bytes.";
            attribute :>> criterion = "Every aperture of every published node is
                answered as minted, the index past the last INVALID_ARGS, and on
                x86-64 pci-testdev's 8 GiB 64-bit BAR above 4 GiB is answered whole
                (the first config line's 1 above 4 GiB, which xtask requires). With
                the length cut to 32 bits the boot fails.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = ("syscall::native::device_aperture",
                "syscall::native::aperture_bytes", "device::Aperture::info",
                "device::Aperture::part", "device::DeviceNode::aperture_info",
                "device::DeviceNode::mint_from", "device::DeviceNode::mint",
                "device::DeviceNode::mint_bar");
        }
        requirement <'L.device.25'> configurationWindow : ItemLowLevel {
            attribute :>> statement = "A driver's configuration write touching any
                byte outside its function's writable ranges shall be refused whole
                with ACCESS_DENIED and change nothing, and a malformed access shall
                be INVALID_ARGS; reads of the function's configuration space shall
                be answered; and a node whose kernel-owned registers read back other
                than the kernel made them, at an accepted HELLO or a quiesce, shall
                have its bus mastering turned off and be refused in one hold of its
                configuration lock, every call needing MANAGE on it answering
                BAD_STATE, and its bus mastering never turned on again.";
            attribute :>> criterion = "Header reads answer what enumeration read,
                and the MSI, MSI-X and extended reads are answered; writes to the
                command register, a BAR, the MSI and MSI-X capabilities, Device
                Control, an uncovered byte and ATS where present are refused and
                read back unchanged; a write spanning a kernel-owned and a writable
                byte is refused; malformed accesses are INVALID_ARGS; a command
                register rewritten behind the kernel is found, bus mastering reads
                off, the node answers BAD_STATE, the line names COMMAND, and an
                enable_dma after it is refused with bus mastering still off (the
                config lines). With the allowlist widened, the whole-range test
                reduced to one byte, verify_config not comparing, or the refused
                node's enable_dma let through, the boot fails.";
            attribute :>> parent = ("H.DMA.5", "H.DMA.3", "H.DEV.4");
            attribute :>> unit = ("device::DeviceNode::config_read",
                "device::DeviceNode::config_write", "device::config_access",
                "device::ConfigState::first_refusal", "device::MappedConfig::read",
                "device::DeviceNode::verify_config", "device::DeviceNode::breach",
                "device::DeviceNode::hello_accepted", "device::DeviceNode::refuse",
                "device::config_breach",
                "device::command_breach", "device::bar_breach", "device::msi_breach",
                "device::msix_control_breach", "device::MsixTable::rewritten",
                "device::Register::fmt",
                "syscall::native::device_config_read", "syscall::native::device_config_write",
                "syscall::native::config_status", "syscall::native::device_in");
        }
        requirement <'L.device.26'> writableRangesAllowlisted : ItemLowLevel {
            attribute :>> statement = "A function's driver-writable configuration
                bytes shall be the bodies of its vendor-specific capabilities, a
                standard one past its three-byte header and a VSEC past its eight,
                cut at the end of their space, less virtio's pci_cfg_data, computed
                at stage 10 from both capability lists; a list that loops, holds a
                capability starting inside a vendor body, or holds more capabilities
                or vendor bodies than the walk records, shall leave the function
                nothing writable.";
            attribute :>> criterion = "ferrix_pci::window's host tests: the header,
                MSI, power management, PCI Express, MSI-X, ATS, AER and uncovered
                bytes are refused and vendor bodies writable; pci_cfg_data is
                refused and virtio's bar, offset and length writable; a body past
                its space is cut; overlapping and looping lists, 65 VSECs and an
                extended list past 304 capabilities leave nothing writable. With the extended list skipped the VSEC test fails. At
                boot virtio-rng's offset field is written and read back and its
                pci_cfg_data refused (the second config line).";
            attribute :>> parent = ("H.DMA.5", "H.DMA.3");
            attribute :>> unit = ("discovery::pci::check_host", "discovery::pci::for_the_window",
                "device::DeviceNode::pci", "device::DeviceNode::window");
        }
        requirement <'L.discovery.1'> oneDescriptionAnsweredOnce : ItemLowLevel {
            attribute :>> statement = "description::of shall read a machine by its
                ACPI tables when they open, whether or not a device tree came too,
                by its device tree when the tables do not open and the tree parses,
                and by neither otherwise; and shall not parse the tree when the
                tables open.";
            attribute :>> criterion = "For ACPI only, both, tables that fail to
                open beside a tree, a tree only, and neither, the description is
                ACPI, ACPI with the tree unopened, the tree, the tree and neither.";
            attribute :>> parent = ("H.DMA.1", "H.DMA.5");
            attribute :>> unit = "discovery::description::of";
        }
        // L.discovery.2-5: the firmware's SMBIOS tables (docs/DMI.md; consultant
        // ledger line 1339). The loader's handover (L.discovery.6, reserved and
        // released unwritten) is .5's assumption: no requirement names a loader
        // unit.
        requirement <'L.discovery.2'> smbiosEntryPointChecked : ItemLowLevel {
            attribute :>> statement = "EntryPoint::parse shall accept only a 3.x
                entry point (_SM3_, length 24 to 32) or a 2.x one (_SM_, length 30
                to 32, with a _DMI_ header at byte 16) whose bytes sum to zero, the
                2.x one's _DMI_ header too, whose table is 1 to 1 MiB long; it shall
                refuse a 2.x entry point whose own checksum fails even when its
                _DMI_ header checks; and it shall answer any bytes without a panic.";
            attribute :>> criterion = "Host tests: good 3.x and 2.x entry points
                parse with Linux's version fixups; a bad checksum, a bad _DMI_
                header, a bad _SM_ checksum beside a good _DMI_, lengths just
                outside both ranges, every truncation, no anchor, an empty table
                and one of 1 MiB and a byte are each refused with their error.";
            attribute :>> parent = "H.BOOT.4";
            attribute :>> unit = ("ferrix_smbios::EntryPoint::parse",
                "ferrix_smbios::EntryPoint::v3", "ferrix_smbios::EntryPoint::v2");
        }
        requirement <'L.discovery.3'> smbiosWalkBounded : ItemLowLevel {
            attribute :>> statement = "walk shall read no byte outside the table it is
                given, shall hand on only a structure whose strings end inside it,
                and shall stop at the count when there is one, at type 127 when there
                is none, at a structure shorter than its header, and where a header
                would not fit.";
            attribute :>> criterion = "Host tests: a length past the end, a structure
                shorter than its header, unterminated strings and a count larger
                than the table stop the walk inside the table; every truncation and
                every single-byte change of a good table walks within bounds; a 3.x
                table stops at type 127 and a 2.x one with a count does not.";
            attribute :>> parent = "H.BOOT.4";
            attribute :>> unit = ("ferrix_smbios::walk", "ferrix_smbios::Smbios::new");
        }
        requirement <'L.discovery.4'> smbiosIdentityAsLinux : ItemLowLevel {
            attribute :>> statement = "Smbios shall decode the identity fields of
                types 0 to 3 from the offsets Linux's dmi_decode reads, the first
                structure of a type winning, and render each as Linux's dmi-id
                shows it, the strings' bytes unaltered.";
            attribute :>> criterion = "Host tests: QEMU's own 2.x and 3.x tables as
                OVMF installed them (src/lib/platform/smbios/src/fixtures.rs, captured
                with xtask's -smbios arguments) read as QEMU built them, and a table
                laid out as QEMU lays out types 0, 1, 3 and 127 reads as Linux shows it, including the
                release, the UUID in both byte orders, the chassis type, empty
                strings, absent fields, bytes passed through, the modalias and the
                cut to a page.";
            attribute :>> parent = "H.BOOT.16";
            attribute :>> unit = ("ferrix_smbios::Smbios::field_text",
                "ferrix_smbios::Smbios::modalias", "ferrix_smbios::decode",
                "ferrix_smbios::string");
        }
        requirement <'L.discovery.5'> smbiosCopyAcceptedOnlyInBounds : ItemLowLevel {
            attribute :>> statement = "discovery::smbios shall read the loader's
                SMBIOS copy only when it is wholly inside one region the memory map
                reports as the SMBIOS copy and wholly inside the direct map, holds
                more than the 32 bytes of the entry point and at most 1 MiB of
                table, and re-parses with a table exactly as long as the entry point
                says; and shall otherwise keep nothing, say why, and let the boot go
                on.";
            attribute :>> criterion = "At boot, forged copies -- none, outside the
                region, straddling its end, below the direct map, overflowing,
                holding no table, a byte longer than its table, a bad checksum --
                are each refused by their name and a good one accepted; stage 1
                requires the map to describe the real copy as the SMBIOS copy.";
            attribute :>> parent = ("H.BOOT.4", "H.BOOT.16");
            attribute :>> unit = ("discovery::smbios::range", "discovery::smbios::decode",
                "discovery::smbios::open");
        }
    }

    package Quiesce {
        doc /* The quiesce in src/kernel/src/syscall/native.rs: every core that
             * serves a device waited out, its DMA turned off, and the claims
             * a core keeps past its driver let go, before the next driver is
             * given it. */

        requirement <'L.quiesce.1'> onlyItsManagerQuiesces : ItemLowLevel {
            attribute :>> statement = "device_quiesce shall be refused with
                ACCESS_DENIED through a device handle without MANAGE.";
            attribute :>> criterion = "A quiesce through a handle to the device
                without MANAGE answers ACCESS_DENIED.";
            attribute :>> parent = "H.OBJ.2";
            attribute :>> unit = "syscall::native::device_quiesce";
        }
        requirement <'L.quiesce.2'> refusedUnderItsDriver : ItemLowLevel {
            attribute :>> statement = "A quiesce shall answer BAD_STATE, changing
                nothing, while a driver serves the device.";
            attribute :>> criterion = "A quiesce of the device a running driver
                serves answers BAD_STATE, and the driver goes on serving it.";
            attribute :>> parent = "H.DEV.2";
            attribute :>> unit = "syscall::native::quiesce_while";
        }
        requirement <'L.quiesce.3'> itWaitsOutADeadDriver : ItemLowLevel {
            attribute :>> statement = "A quiesce made as a driver dies shall wait
                for every core serving the device to let go, turn the device's DMA
                off, let go the claims a core keeps past its driver, and answer
                success.";
            attribute :>> criterion = "A quiesce the instant a block driver's
                handles close answers success, and the next driver is given a ring
                on the device.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = "syscall::native::quiesce_while";
        }
        requirement <'L.quiesce.4'> aRestartedManagerQuiesces : ItemLowLevel {
            attribute :>> statement = "The kernel shall quiesce every device a dead
                devmgr's drivers served before a devmgr started again is handed
                them.";
            attribute :>> criterion = "Restarting devmgr.service starts a second
                devmgr, which is given the devices and starts their drivers.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = "syscall::native::quiesce";
        }
        requirement <'L.quiesce.5'> theWaitHasAnEnd : ItemLowLevel {
            attribute :>> statement = "A quiesce whose device a core has not let go
                within its patience shall answer TIMED_OUT, and one whose DMA
                cannot be turned off BAD_STATE.";
            attribute :>> criterion = "A quiesce with a core that never lets go
                answers TIMED_OUT; one on a function whose configuration space
                cannot be mapped answers BAD_STATE.";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = "syscall::native::quiesce_while";
        }
    }

    package IsolatedInterrupts {
        doc /* NVIDIA's N0g (docs/NVIDIA.md section 12.3, condition G5): the
             * isolated-interrupts mark, device_isolation, and no vector for a
             * function whose messages arrive under another's source ID. */

        requirement <'L.device.27'> aMarkedDeviceNeedsIsolatedInterrupts : ItemLowLevel {
            attribute :>> statement = "device_set_limit's
                DEVICE_LIMIT_ISOLATED_INTERRUPTS shall set a node's
                isolated-interrupts mark through a handle with SET_LIMIT, and a
                value that would clear a set mark shall be refused ACCESS_DENIED;
                nothing else shall clear it. While a node's mark is set and its
                interrupts are not isolated, interrupt_create and vmo_pin shall be
                refused ACCESS_DENIED. device_isolation shall answer bit 0 for a
                translated domain and bit 1 for isolated interrupts. devmgr shall
                start no driver of a device that runs firmware of its own whose
                mark the kernel refused.";
            attribute :>> criterion = "On x86-64 check R10 sets edu's mark,
                reads device_isolation 0x3, is given a vector, is refused one
                ACCESS_DENIED while the machine's interrupts are forced not
                isolated, and is refused clearing the mark; with the refusal
                skipped R10 panics. devmgr-proto's host tests: a firmware device
                starts only once marked, with its line when refused; with the
                refusal ignored they fail.";
            attribute :>> parent = ("H.DMA.9", "H.DEV.1");
            attribute :>> unit = ("syscall::native::set_isolated_mark",
                "syscall::native::set_limit_on", "syscall::native::device_set_limit",
                "syscall::native::device_get_limit", "syscall::native::device_isolation",
                "syscall::native::device_isolated_in", "syscall::native::interrupt_create",
                "syscall::native::vmo_pin", "device::DeviceNode::interrupts_isolated",
                "device::DeviceNode::may_take_vectors_and_pins");
        }
        requirement <'L.device.28'> noVectorUnderAnothersSourceId : ItemLowLevel {
            attribute :>> statement = "While a unit remaps, a function the
                remapping units do not place as its own requester -- behind no unit,
                or under a bridge's alias -- shall be given no message route, and
                so no vector, and the boot shall name each such function.";
            attribute :>> criterion = "On x86-64 a requester no unit places is
                refused a route (the remap line); with the refusal turned into a
                compatibility route the boot panics.";
            attribute :>> parent = "H.DMA.9";
            attribute :>> unit = ("iommu::remapping::route",
                "arch::x86_64::msi::msi_allocate", "iommu::name_functions_without_vectors");
        }
        requirement <'L.device.35'> heldOffFunctionGetsItsTablePage : ItemLowLevel {
            attribute :>> statement = "A PCI function in the build's MSI-preferred
                list (ferrix_pci::msix::MSI_PREFERRED, exactly 1F0A:6801) with both an
                MSI-X table and an MSI capability, its configuration space placed, no
                INTx line and its memory decoded shall have its MSI-X held off at
                stage 10 before its node is built -- Enable written 0 and Function
                Mask 1, and read back so -- and only then be granted its table and
                pending-bit pages as aperture, offered no MSI-X table and its MSI
                capability's one vector; one that does not read back so keeps its
                table withheld and its MSI-X offered.";
            attribute :>> criterion = "On x86-64 test-boot requires the msix-off
                line, from the check run on a scratch node enumeration builds for
                QEMU's e1000e with QEMU's subsystem as if it were listed, and never
                publishes: Enable reads 0 and Function Mask 1, the table's and the
                pending-bit array's whole ranges and their first and last bytes lie
                in its apertures, as do the table pages it records, it offers no
                MSI-X table and one vector, and its MSI mints once, masked, on a number with no
                handler. Host tests: the list is exactly 1F0A:6801, the check case
                refuses 8086:10D3 with any subsystem but QEMU's, and hold_off reads
                back Enable 0 and Function Mask 1. With the hold skipped, the table
                still withheld, or the MSI vector not offered, the boot fails. The
                1F0A:6801 arm is reached only on the hardware.";
            attribute :>> parent = ("H.DMA.5", "H.IRQ.1");
            attribute :>> unit = ("discovery::pci::held_off", "discovery::pci::hold_msix_off",
                "device::DeviceNode::pci", "device::MsiFunction::of");
        }
        requirement <'L.device.36'> heldOffMsixReadBack : ItemLowLevel {
            attribute :>> statement = "DeviceNode::verify_config shall refuse a node
                whose MSI-X the kernel holds off when its MSI-X Enable reads 1, naming
                MSI-X control; the Function Mask the kernel wrote is not compared,
                since the chip's own reset may clear it while Enable stays 0.";
            attribute :>> criterion = "On x86-64, with Enable written 1 on the
                scratch node of L.device.35 -- the mirror case -- verify_config
                reports MSI-X control and the node reads refused (the msix-off line);
                with the comparison removed the boot fails.";
            attribute :>> parent = ("H.DMA.5", "H.IRQ.1");
            attribute :>> unit = ("device::DeviceNode::verify_config",
                "device::msix_off_breach");
        }
        requirement <'L.device.37'> noMsiWhileMsixOn : ItemLowLevel {
            attribute :>> statement = "MsiFunction::mint shall refuse a function
                whose MSI-X the kernel holds off when its MSI-X Enable, read under the
                node's configuration lock at the mint, reads 1.";
            attribute :>> criterion = "On x86-64 the scratch node of L.device.35,
                with Enable written 1, is refused its MSI vector when asked again
                (the msix-off line); with the refusal removed the boot fails.";
            attribute :>> parent = "H.IRQ.1";
            attribute :>> unit = "device::MsiFunction::mint";
        }
        requirement <'L.device.38'> theFirmwareFramebufferLentExactly : ItemLowLevel {
            attribute :>> statement = "The boot framebuffer shall stay in the
                ranges withheld from every aperture but one, withheld as its size
                in whole pages: on x86-64 under ferrix.efifb, and only there, at
                most one node, whose one aperture starts at the framebuffer's
                first byte and is its visible lines in whole pages, prefetchable,
                with no vector. None shall be made when the framebuffer is in
                memory the allocator owns, its size does not hold its lines, its
                layout is not four bytes a pixel, its first pixel is off a page,
                or another reserved range lies inside the lines' whole pages; and
                an aperture of the node's kind shall be withheld when it reaches
                past the framebuffer's size in whole pages or lies in a reserved
                range that is not the framebuffer.";
            attribute :>> criterion = "Every boot lends a made-up 1024x768
                framebuffer exactly its visible lines as one aperture of its own
                source, prefetchable; refuses one crossed by another reservation, one the
                allocator owns, one a page short, one of an unknown layout and
                one off a page; withholds a lent aperture reaching past the
                framebuffer and one inside another reserved range; and withholds
                an ordinary window at the framebuffer. It lends a made-up 800x600
                framebuffer whose size is exactly its 1,920,000 visible bytes its
                lines in whole pages (469), refuses it one byte short, and
                withholds it when another range starts in its last page. A node without the option
                or off x86-64 fails the boot. Under ferrix.efifb the published
                node holds one prefetchable aperture equal to the framebuffer's
                lines and no vector (test-compositor --boot firmware-screen). With
                the exemption granted to any aperture, or either clause of
                Reserved::lends dropped, or a node made without the option, or
                the aperture minted not prefetchable, the boot panics.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = ("device::DeviceNode::firmware_framebuffer",
                "device::framebuffer_span",
                "device::Reserved::lends", "device::Reserved::covers",
                "device::Reserved::synthetic", "device::Reserved::framebuffer",
                "discovery::firmware::FirmwareFramebuffer::new",
                "discovery::firmware::FirmwareFramebuffer::lend",
                "discovery::firmware::FirmwareFramebuffer::find",
                "discovery::firmware::may_lend");
        }
        requirement <'L.device.39'> theFirmwareFramebufferTakesNoPin : ItemLowLevel {
            attribute :>> statement = "The firmware framebuffer's node shall reach
                no memory: it shall get no domain; vmo_pin, plain or contiguous,
                and device_set_limit on it shall answer WRONG_TYPE; and before the
                node is published the kernel shall take, and never give back, a
                hold of write-combining, its own window's memory type
                (L.device.43), over the aperture, so that every user mapping of
                it is held to that type (L.user.109) and only
                io_mapping_map_combining maps it.";
            attribute :>> criterion = "Under ferrix.efifb the node's domain is
                refused and none was made, its DMA is off, a hold of the aperture
                as device or cached memory is refused while one as
                write-combining is not, and vmo_pin (plain and PIN_CONTIGUOUS)
                and device_set_limit(PIN_PAGES) through the calls answer
                WRONG_TYPE; io_mapping_map of the window is refused and
                io_mapping_map_combining given, its user leaf write-combining and
                not device (W8; test-compositor --boot firmware-screen). With the
                node reaching memory, vmo_pin's refusal removed, the hold not
                taken or taken as device memory, the boot panics.";
            attribute :>> parent = ("H.DMA.3", "H.DMA.5");
            attribute :>> unit = ("device::DeviceNode::reaches_memory",
                "device::DeviceNode::domain", "discovery::firmware::refuses",
                "syscall::native::vmo_pin", "syscall::native::set_limit_on");
        }
        requirement <'L.device.40'> theHolderOfALentFramebufferIsNotDriven : ItemLowLevel {
            attribute :>> statement = "While the firmware framebuffer is lent to
                its own node, every PCI function one of whose BARs holds it shall
                be driven by nobody: every native call that needs MANAGE on a
                device reaches it through device_in, and on such a function each
                shall answer BAD_STATE, configuration reads and I/O mappings
                included.";
            attribute :>> criterion = "Under ferrix.efifb every function whose
                BAR ranges, read again from the published nodes, overlap the
                framebuffer is marked; the VGA function that holds it answers a
                configuration read and an io_mapping_create BAD_STATE (W2 and W8,
                the config line: one function); and no other node's aperture
                overlaps the framebuffer (test-compositor --boot
                firmware-screen). With the function left driven, or the mark
                minting leaves never set, the boot panics.";
            attribute :>> parent = ("H.DMA.5", "H.DEV.1");
            attribute :>> unit = ("device::yield_framebuffer_holder",
                "device::DeviceNode::yielded", "device::DeviceNode::mint_bar",
                "syscall::native::device_in");
        }
        requirement <'L.device.41'> theBootConsoleLetsGoFirst : ItemLowLevel {
            attribute :>> statement = "The boot console shall stop drawing on the
                framebuffer, waiting out a draw in flight, before the firmware
                framebuffer's node exists, so that no kernel draw but a panic's
                meets a ring-3 driver's on its pages.";
            attribute :>> criterion = "Under ferrix.efifb ferrix.fbcon the console
                draws the boot until the node is made, and stage 10's check of the
                published node finds it stopped; hyprix's picture on the
                framebuffer then holds no console pixel (test-compositor --boot
                firmware-screen). With the stop moved after the node is
                published, the boot panics.";
            attribute :>> parent = ("H.DMA.5", "H.FAIL.1");
            attribute :>> unit = ("console::screen::stop",
                "discovery::firmware::FirmwareFramebuffer::lend");
        }
        requirement <'L.device.42'> theFirmwareFramebufferModeDescribed : ItemLowLevel {
            attribute :>> statement = "DeviceNode::describe shall report the
                firmware framebuffer's node as TREE_FIRMWARE_FB with one aperture
                and no vector, and its width, height, stride and pixel layout, as
                it was minted, in the fields a tree node leaves zero.";
            attribute :>> criterion = "Under ferrix.efifb the node's device_info,
                read back through the call, names TREE_FIRMWARE_FB, one aperture,
                no vector and the minted mode (W8, the config line;
                test-compositor --boot firmware-screen), and efifb starts at the
                firmware's mode. With the mode fields not written, the boot
                panics.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = "device::DeviceNode::describe";
        }
        requirement <'L.device.43'> theLentFramebufferWriteCombining : ItemLowLevel {
            attribute :>> statement = "On x86-64 under ferrix.efifb the firmware
                framebuffer's node shall not be made unless every processor
                programmed its PAT (L.x86_64.127) and every page of the kernel's
                panic window over the framebuffer reads back write-combining,
                writable, not user's and none as device memory (L.x86_64.176),
                read before the boot console is stopped and the kernel's hold is
                taken.";
            attribute :>> criterion = "Under ferrix.efifb stage 10's check of the
                published node reads every page of the kernel's window back
                write-combining, not device, writable and not user's, as the
                type the hold keeps; reads this processor's IA32_PAT back as
                X86_PAT and every processor counted as programmed; and efifb
                says it mapped the window write-combining (test-compositor
                --boot firmware-screen). The lend's read-back answers false on a
                device page (stage 9, L.x86_64.176); with it answering true for
                any page, the boot panics.";
            attribute :>> parent = ("H.MEM.15", "H.DMA.5");
            attribute :>> unit = ("discovery::firmware::FirmwareFramebuffer::lend",
                "discovery::firmware::window_combining", "panic::screen::window",
                "arch::x86_64::framebuffer::pat",
                "arch::x86_64::framebuffer::mtrr_type",
                "arch::x86_64::framebuffer::drain_write_combining");
        }
    }
}
