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
                published aperture does.";
            attribute :>> criterion = "Every aperture of every published node is
                clear of every reserved range.";
            attribute :>> parent = ("H.MEM.10", "H.DMA.5");
            attribute :>> unit = ("device::DeviceNode::mint", "device::Reserved::covers",
                "device::Aperture::overlaps");
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
                grants.";
            attribute :>> criterion = "For every published PCI function, the whole
                of each MSI-X table or pending-bit range, its first byte and its
                last byte are refused.";
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
                apertures overlap, and publish no device tree vector that is not a
                shared peripheral interrupt or that two nodes hold.";
            attribute :>> criterion = "Sorted by start, no two published apertures
                overlap; every device tree vector is 32 or above and held by one
                node.";
            attribute :>> parent = ("H.DMA.5", "H.IRQ.1");
            attribute :>> unit = "device::publish";
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
                configuration space, its command register reads bus mastering and
                decoding on after enable_dma, and bus mastering off with decoding
                on after disable_dma.";
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
                MSI-X table shall be offered one vector, minted once with INTx off --
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
    }
}
