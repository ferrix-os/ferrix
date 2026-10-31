/*
 * Ferrix — SysML v2 model, part 17: the low-level requirements of memory.
 *
 * What each unit of the item's memory management does, as `ItemLowLevel`
 * requirements (part 13 defines the format, part 14 is the pilot this
 * copies), in two id spaces.
 *
 *  - `L.mm.<n>`: the kernel's own memory -- src/kernel/src/mm.rs and mm/ (the
 *    frame allocator's glue, the heap, the kernel's page tables, the
 *    allocation reserve, page tables unlinked and waiting for their
 *    shootdown), vmap.rs (the kernel address arena, device windows and
 *    kernel stacks) and early.rs (the mapper stage 1 runs before there is an
 *    allocator);
 *  - `L.user.<n>`: what a program's memory is made of -- src/kernel/src/user/
 *    vmo.rs (memory objects: their pages, copy-on-write, sharing and
 *    retirement) and user/space.rs (address spaces: mappings, faults,
 *    protection, unmapping and its shootdown, and the charge for them).
 *
 * Two id spaces rather than one, because they are two layers: nothing in
 * `L.user` is reached before the kernel's own memory is up, and a check of a
 * user address space is not evidence for the kernel's. Ids are flat within
 * each, the package gives the file, and they are never renumbered once a
 * check names them.
 *
 * L.user.173 was reserved for the tmpfs root's release after a switch to a
 * root disk (docs/RAM-RELEASE.md §2) and released unwritten: that is
 * fs/mod.rs and ferrix-vfs, the load ring and no ring, and no item code
 * relies on what the tmpfs holds after it (consultant ruling, ledger 5038).
 *
 * The frame, heap, paging and vma arithmetic itself is src/lib/kernel/frame,
 * heap, paging and vma, whose host tests verify it; nothing here restates
 * them. What is here is what the kernel does with them.
 *
 * The pilot's rules hold: a requirement names every function that carries
 * its behaviour; it is no broader than one check can prove, and where the
 * checks that exist each prove a part it is split; an accessor needs no
 * requirement of its own; and check code living in a product file is listed
 * in tools/common/data/traceability-units.json, not required.
 */
package FerrixMemoryRequirements {
    private import FerrixItemRequirements::*;

    package MemoryManagement {
        doc /* src/kernel/src/mm.rs: the frame allocator's interface, the kernel heap's
             * page supply and allocator, the kernel's and other trees' page tables,
             * and frame zeroing and copying. The buddy allocator and the heap
             * themselves are src/lib/kernel/frame and src/lib/kernel/heap, whose host tests
             * prove them; nothing here restates them. The frame windows at the end
             * of the file serve the checks only (mm-draft-A-notes.md). */

        requirement <'L.mm.1'> initGivesTheAllocatorTheUsableRam : ItemLowLevel {
            attribute :>> statement = "init shall give the frame allocator every frame
                firmware reported usable below the direct map's limit, except frame 0
                and the pages the per-frame array occupies, and shall refuse a memory
                map with no usable RAM, or with no usable region inside the direct map
                that holds the array, before it hands anything over.";
            attribute :>> criterion = "For memory maps built for the purpose, the
                frames the allocator manages equal the usable frames below the direct
                map's limit less frame 0 and the array's pages, 0 more and 0 fewer; a
                map with no usable region is refused NoUsableMemory and one whose
                usable regions are all smaller than the array NoRoomForPageArray.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("mm::init", "mm::place_page_array",
                "mm::insert_region", "mm::lowest_ram_frame", "mm::MemoryError::fmt");
        }
        requirement <'L.mm.2'> frameZeroIsNeverHandedOut : ItemLowLevel {
            attribute :>> statement = "init shall leave frame 0 out of the frames it
                gives the allocator, on every architecture, so that physical address 0
                is never handed out (FX-0601).";
            attribute :>> criterion = "After memory bring-up the allocator records
                frame 0 as Reserved or holds no record of it, and a request for a frame
                lying below frame 1 is refused.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("mm::init", "mm::insert_region");
        }
        requirement <'L.mm.3'> aFrameHasOneHolder : ItemLowLevel {
            attribute :>> statement = "allocate_frames shall not hand out a frame a
                live allocation holds, and shall align a block of 2^order frames to
                2^order frames.";
            attribute :>> criterion = "Two single frames taken in turn are different
                frames, and a block of order 4 starts on a multiple of 16 frames.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("mm::allocate_frames", "mm::with_frames");
        }
        requirement <'L.mm.4'> freedBlocksComeBack : ItemLowLevel {
            attribute :>> statement = "deallocate_frames shall return every block
                allocate_frames handed out to the free frames, whatever order blocks of
                different sizes are given back in.";
            attribute :>> criterion = "16 rounds of up to 256 blocks of orders 0 to 4,
                each round giving back every other block first and then the rest, leave
                the free frame count exactly where it started.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::allocate_frames", "mm::deallocate_frames");
        }
        requirement <'L.mm.5'> aRunIsWholeBlocks : ItemLowLevel {
            attribute :>> statement = "allocate_frame_run shall hand out blocks of
                2^MAX_ORDER frames lying back to back, each an allocated block that can
                be given back on its own.";
            attribute :>> criterion = "A run of 2 blocks has both block heads
                allocated and each is given back alone with deallocate_frames; the
                frame count user/check.rs's run takes around the stage 6 checks comes
                back to where it started.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("mm::allocate_frame_run", "mm::deallocate_frames");
        }
        requirement <'L.mm.6'> aSplitBlockIsSingleFrames : ItemLowLevel {
            attribute :>> statement = "split_frames shall turn an allocated block into
                single allocated frames, each of which release_frame gives back on its
                own.";
            attribute :>> criterion = "A block of 2 frames split with split_frames
                leaves both frames allocated, and release_frame of each answers that it
                went back to the allocator, 2 of 2.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::split_frames", "mm::release_frame");
        }
        requirement <'L.mm.7'> aSharedFrameOutlivesOneHolder : ItemLowLevel {
            attribute :>> statement = "share_frame shall count another reference to a
                frame, and release_frame shall return the frame to the allocator only
                when it drops the last reference, answering whether it did.";
            attribute :>> criterion = "A page committed to a VMO and shared once reads
                2 references; with the VMO dropped it reads 1 and the held frame count
                is one below its start; release_frame of the last reference answers
                true and the count is back where it started.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::share_frame", "mm::release_frame",
                "mm::frame_references");
        }
        requirement <'L.mm.8'> userFramesAreChargedToTheirJob : ItemLowLevel {
            attribute :>> statement = "allocate_user_frame shall charge the frame to
                the running task's job, refusing the frame when the charge is refused,
                and the frame's release shall take the charge back from that job.";
            attribute :>> criterion = "Faults made as a task of a job limited to 48
                pages are refused at exactly its limit while a sibling job faults 48
                pages in, and with both spaces dropped every job's memory reads 0.";
            attribute :>> parent = ("H.QUOTA.2", "H.QUOTA.4");
            attribute :>> unit = ("mm::allocate_user_frame", "mm::release_frame",
                "mm::deallocate_frames", "mm::allocate_user_run");
        }
        requirement <'L.mm.9'> userTablesAreChargedToTheirJob : ItemLowLevel {
            attribute :>> statement = "A page table map_in makes for a user mapping
                shall be charged to the running task's job, refused with the mapping
                when the charge is refused, and the charge taken back when the table is
                freed.";
            attribute :>> criterion = "A user mapping made as a task of a job, needing
                N new page tables, raises the job's memory by N pages beyond its frames;
                at a limit one page short of them it is refused, 0 tables left made;
                with the tree torn down the job's memory reads 0.";
            attribute :>> parent = ("H.QUOTA.2", "H.QUOTA.4");
            attribute :>> unit = ("mm::ChargedTables::allocate_table", "mm::map_in",
                "mm::deallocate_frames");
        }
        requirement <'L.mm.10'> aDisownedFrameIsChargedToNobody : ItemLowLevel {
            attribute :>> statement = "disown_frame shall take a held frame's charge
                off the job it was charged to and leave the frame allocated and charged
                to nobody.";
            attribute :>> criterion = "A frame allocated as a task of a job and then
                disowned leaves the job's memory where it was before the allocation,
                the frame still allocated; its release afterwards changes no job's
                memory.";
            attribute :>> parent = ("H.QUOTA.4", "H.DMA.4");
            attribute :>> unit = "mm::disown_frame";
        }
        requirement <'L.mm.11'> lowFramesStayBelowTheirLimit : ItemLowLevel {
            attribute :>> statement = "allocate_frames_below shall hand out a block
                lying wholly below the frame limit it is given, or none.";
            attribute :>> criterion = "Every block of orders 0 to 2 taken below a
                limit of 256 frames ends at or below frame 256, until a request is
                refused; all of them given back, the free count is unchanged.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = "mm::allocate_frames_below";
        }
        requirement <'L.mm.12'> theHeapHoldsWhatIsPutInIt : ItemLowLevel {
            attribute :>> statement = "The kernel heap shall hand each allocation a
                block that holds what is written to it until it is freed, through
                growth and reallocation.";
            attribute :>> criterion = "A Box holds the value put in it; a Vec pushed
                to 4096 values through its reallocations holds each value at its index;
                a BTreeMap given 2048 inserts returns every value it holds under the key
                it was inserted with; and the heap has taken at least one page.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("mm::KernelAllocator::alloc",
                "mm::KernelAllocator::dealloc", "mm::KernelPages::allocate_pages",
                "mm::KernelPages::read_link", "mm::KernelPages::write_link",
                "mm::with_heap");
        }
        requirement <'L.mm.13'> theHeapGivesItsPagesBack : ItemLowLevel {
            attribute :>> statement = "The kernel heap shall count every freed block
                as free and give the slab pages it no longer needs back to the frame
                allocator, keeping at most one per size class.";
            attribute :>> criterion = "Around the heap check -- a Box, a Vec grown to
                4096 values and a BTreeMap of 2048 inserts, all dropped -- the heap's
                live bytes come back to what they were, and its pages grow by no more
                than ferrix_heap::CLASSES.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::KernelAllocator::dealloc",
                "mm::KernelPages::deallocate_pages", "mm::heap_allocated",
                "mm::heap_pages");
        }
        requirement <'L.mm.14'> aRefusedAllocationIsNullAndCounted : ItemLowLevel {
            attribute :>> statement = "KernelAllocator::alloc shall answer an
                allocation that neither the heap nor an open section's reserve can
                serve with a null pointer, and count it as a refusal.";
            attribute :>> criterion = "With the heap unable to take a page and no
                section open, a fallible allocation is refused AllocError and
                heap_refusals rises by exactly 1; the heap's live bytes are unchanged.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::KernelAllocator::alloc", "mm::heap_refusals");
        }
        requirement <'L.mm.15'> pageTablesAreZeroed : ItemLowLevel {
            attribute :>> statement = "A page table the kernel's mapper is given, for
                the kernel's own tables or another tree's, shall hold only zeros.";
            attribute :>> criterion = "A frame filled with a pattern and given back is
                claimed again as a new table by a mapping; all 512 of its descriptors
                other than the one the mapping wrote read 0.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = ("mm::KernelPhysMem::allocate_table",
                "mm::ChargedTables::allocate_table");
        }
        requirement <'L.mm.16'> zeroFrameClearsTheWholeFrame : ItemLowLevel {
            attribute :>> statement = "zero_frame shall write zero to every byte of
                the frame it is given.";
            attribute :>> criterion = "A frame filled with a non-zero pattern and
                passed to zero_frame reads 0 in 4096 of its 4096 bytes.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = "mm::zero_frame";
        }
        requirement <'L.mm.17'> copyFrameCopiesTheWholeFrame : ItemLowLevel {
            attribute :>> statement = "copy_frame shall make every byte of the
                destination frame equal to the same byte of the source.";
            attribute :>> criterion = "A frame filled with a pattern that differs at
                every word, copied to a frame filled with another, reads the first
                pattern in 4096 of its 4096 bytes, and the source is unchanged.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = "mm::copy_frame";
        }
        requirement <'L.mm.18'> demandPagesAreMappedOnFault : ItemLowLevel {
            attribute :>> statement = "map_demand_page shall resolve a kernel fault in
                the on-demand window by mapping one fresh frame at the faulting page,
                so that the access completes when it is retried.";
            attribute :>> criterion = "Writes to 3 unmapped pages of the window, out of
                order, each fault once, read back what was written and leave a mapping,
                consuming between 3 and 6 frames; a fourth into a region whose tables
                exist costs exactly 1 frame.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = ("mm::map_demand_page", "mm::is_demand_window");
        }
        requirement <'L.mm.19'> translateInAgreesWithTheProcessor : ItemLowLevel {
            attribute :>> statement = "translate_in shall answer, for an address in a
                tree, the physical address the processor reaches through that address
                while the tree is installed.";
            attribute :>> criterion = "In each of the 2 rounds
                check_the_processor_walks_an_installed_space runs, a value written
                through each of a two-page space's user addresses, the space installed,
                is read back through it and at the frame translate_in names for it
                through the direct map, 2 of 2 pages.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("mm::translate_in", "mm::KernelPhysMem::read");
        }
        requirement <'L.mm.20'> mapInMapsWhatItIsAsked : ItemLowLevel {
            attribute :>> statement = "map_in shall map len bytes, rounded up to whole
                pages, at virt onto phys in the tree rooted at root, with the
                permissions asked for, and nothing else.";
            attribute :>> criterion = "A mapping of 3 pages made with map_in into an
                empty tree translates each page to phys plus its offset with the flags
                asked for, and the pages before and after it do not translate.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = ("mm::map_in", "mm::KernelPhysMem::write",
                "mm::KernelPhysMem::allocate_table");
        }
        requirement <'L.mm.21'> userCodeIsFetchedAsWritten : ItemLowLevel {
            attribute :>> statement = "Before it maps a range user-executable, map_in
                shall make what was written to its frames through the direct map what
                every processor fetches from them.";
            attribute :>> criterion = "On each architecture and on a multi-core Arm
                board, code written into a frame through the direct map, then mapped
                user-executable with map_in and run by a program on another processor,
                returns the value the new code computes, N of N runs.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = "mm::map_in";
        }
        requirement <'L.mm.22'> kernelSlotsAreShared : ItemLowLevel {
            attribute :>> statement = "share_kernel_slots shall make the given top
                level slots of a tree point where the kernel root's do.";
            attribute :>> criterion = "On x86-64, every upper-half slot of a new
                address space's root equals the kernel root's, and a kernel address
                translates through the new root to what it does through the kernel's.";
            attribute :>> parent = "H.MEM.2";
            attribute :>> unit = "mm::share_kernel_slots";
        }
        requirement <'L.mm.23'> unmapInHoldsItsTablesForTheShootdown : ItemLowLevel {
            attribute :>> statement = "unmap_in shall take a mapping down from a live
                tree and put every page table the removal empties on the shootdown's
                list, giving none of them back itself (finding F-36).";
            attribute :>> criterion = "An unmap_in of the one page mapped in a fresh
                tree's user half leaves the page untranslated, lists at least one
                emptied table, counts 0 user tables given back, and leaves every listed
                table allocated: claim_frame refuses each.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("mm::unmap_in", "mm::unlinked::UnlinkedTables::push");
        }
        requirement <'L.mm.24'> unwalkedTablesGoBackAtOnce : ItemLowLevel {
            attribute :>> statement = "unmap_unwalked shall take a mapping down from a
                tree no processor can walk and give every page table the removal
                empties back to the frame allocator before it returns.";
            attribute :>> criterion = "A 3-page mapping made in a fresh tree and taken
                down with unmap_unwalked leaves nothing translating, raises the user
                tables given back by the tables the mapping made, and the free frame
                count returns to what it was before the mapping.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "mm::unmap_unwalked";
        }
        requirement <'L.mm.25'> pruneInGivesBackEmptyTables : ItemLowLevel {
            attribute :>> statement = "prune_in shall give back every page table in
                the range of a tree that maps nothing, and no table that maps
                something.";
            attribute :>> criterion = "In a tree where a map_in failed part way,
                leaving tables above the page it could not map, prune_in over the range
                gives back exactly those tables, and a page mapped elsewhere in the tree
                still translates.";
            attribute :>> parent = ("H.MEM.9", "H.MEM.11");
            attribute :>> unit = "mm::prune_in";
        }
        requirement <'L.mm.26'> ioPagesTranslateWhileMapped : ItemLowLevel {
            attribute :>> statement = "map_io shall map one 4 KiB page at an I/O
                address in an IOMMU tree, with no block mapping, so that translate_io
                walking the tree as the unit does finds that page there.";
            attribute :>> criterion = "Two frames mapped into a translated domain are
                each found by translate_io at their own I/O address, and every leaf of
                the domain's tree is 4 KiB.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("mm::map_io", "mm::translate_io");
        }
        requirement <'L.mm.27'> ioTablesWaitForTheUnit : ItemLowLevel {
            attribute :>> statement = "unmap_io shall take the page down and put every
                page table the removal empties on the caller's list, giving none of
                them back itself.";
            attribute :>> criterion = "An unmap_io of the one page mapped in a domain's
                tree leaves it untranslated by translate_io, lists at least one emptied
                table, and every listed table is still allocated until the list is
                released.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "mm::unmap_io";
        }
        requirement <'L.mm.28'> aKernelUnmapIsSeenEverywhere : ItemLowLevel {
            attribute :>> statement = "When unmap_kernel returns, no online processor
                shall still reach the range through the mapping it removed.";
            attribute :>> criterion = "Over 20 rounds in which every online processor
                reads a kernel page and the page is then moved to another frame with
                unmap_kernel and map_kernel, every processor reads the page's new value
                each round: 0 reads through a stale translation.";
            attribute :>> parent = "H.MEM.12";
            attribute :>> unit = ("mm::unmap_kernel", "mm::unmap_kernel_all",
                "mm::unmap_and_release", "mm::map_kernel", "mm::with_tables");
        }
        requirement <'L.mm.29'> aKernelUnmapReleasesAfterItsShootdown : ItemLowLevel {
            attribute :>> statement = "unmap_and_release shall hand the caller the
                frames a kernel unmap removed, and give back the page tables it
                emptied, only after the shootdown covering the removal has returned,
                an unmap that failed part way included.";
            attribute :>> criterion = "For an unmap of a vmap allocation, every frame
                passed to the release closure and every table given back is passed or
                given after the shootdown count has risen by one, 0 before; the same
                for an unmap made to fail part way.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("mm::unmap_and_release", "mm::Deferred::push");
        }
        requirement <'L.mm.30'> aKernelUnmapNeedsNoMemory : ItemLowLevel {
            attribute :>> statement = "unmap_kernel_all shall make room for all it may
                release before it unmaps anything and, with no room, unmap in pieces of
                at most 32 pages, each shot down and released before the next, losing
                nothing it releases.";
            attribute :>> criterion = "An unmap of a 100-page vmap allocation with
                every fallible allocation failing removes all 100 pages, gives every
                frame and table back (0 frames kept, LOST_TO_UNMAPS 0) and takes at
                least 4 shootdowns.";
            attribute :>> parent = ("H.MEM.11", "H.MEM.9");
            attribute :>> unit = ("mm::unmap_kernel_all", "mm::unmap_kernel_in_pieces",
                "mm::Deferred::make_room", "mm::Deferred::push");
        }
        requirement <'L.mm.31'> protectChangesTheTables : ItemLowLevel {
            attribute :>> statement = "protect_kernel shall change what an existing
                kernel mapping permits in the page tables themselves, leaving it
                mapped.";
            attribute :>> criterion = "A fresh vmap page's leaf reads writable;
                protected KERNEL_RODATA it reads not writable and still translates; and
                protecting it KERNEL_DATA again is accepted.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = ("mm::protect_kernel", "mm::with_tables");
        }
        requirement <'L.mm.32'> noKernelMappingIsWritableAndExecutable : ItemLowLevel {
            attribute :>> statement = "Once bring-up has dropped the loader's identity
                map, no leaf under the kernel's root, nor under the identity map's own
                root where the architecture has one, shall be both writable and
                executable.";
            attribute :>> criterion = "The sweep of every leaf under those roots, after
                the identity map is dropped, counts N leaves, E > 0 of them executable,
                and 0 both writable and executable (the w^x line).";
            attribute :>> parent = "H.MEM.4";
            attribute :>> unit = ("mm::map_kernel", "mm::protect_kernel",
                "mm::clear_root_slots", "early::EarlyMemory::map_window");
        }
        requirement <'L.mm.33'> theImageIsWritableNowhere : ItemLowLevel {
            attribute :>> statement = "No leaf under the kernel's root that maps a
                frame of the image's text or read-only data shall be writable, and the
                direct map shall alias every byte of them read-only.";
            attribute :>> criterion = "The sweep of every leaf under the kernel's root
                finds the image's text and read-only data non-empty, 0 of the leaves
                mapping them writable, and the direct map's leaves over them covering
                every byte (the sealed line).";
            attribute :>> parent = "H.MEM.5";
            attribute :>> unit = ("mm::direct_map", "mm::overlaps_image",
                "vmap::map_device", "early::EarlyMemory::map_window");
        }
        requirement <'L.mm.34'> earlyBootMemoryIsReclaimed : ItemLowLevel {
            attribute :>> statement = "reclaim_boot_memory shall give the frame
                allocator the loader's and the ACPI-reclaimable frames the per-frame
                array covers, above frame 0, and none of the page tables, boot stack or
                boot information the kernel still uses.";
            attribute :>> criterion = "The frames reclaimed equal the loader and
                ACPI-reclaim frames of the memory map inside the array, above frame 0,
                and greater than 0; afterwards no frame of a PageTables, BootStack or
                BootInfo region is free.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = "mm::reclaim_boot_memory";
        }
        requirement <'L.mm.75'> thePstoreRegionIsNeverFree : ItemLowLevel {
            attribute :>> statement = "No frame of a Pstore region shall be free at
                boot, reclaimed, or on a free list.";
            attribute :>> criterion = "After reclaim_boot_memory, every frame of the
                attached region (257 for the 1 MiB ring and its header) reads
                Reserved from the frame allocator, or lies outside its array: none
                Free, FreeTail, Allocated or AllocatedTail, on every boot that
                attached one.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = "mm::reclaim_boot_memory";
        }
        requirement <'L.mm.91'> aRetiredFrameIsNeverAllocatedAgain : ItemLowLevel {
            doc /* docs/MACHINE-CHECK.md §3.4: retirement by late kill; the
                insert, the claim and the free paths' test are under the frame
                allocator's lock (the consultant's M4). */
            attribute :>> statement = "retire_frame shall, in one hold of the
                frame allocator's lock, record the frame and take it off the free
                lists if it is free; release_frame and deallocate_frames shall,
                under that lock, keep a recorded frame allocated and charged to
                nobody instead of freeing it, freeing the rest of a block that
                holds one frame by frame; and a frame the allocator does not
                cover shall not be recorded.";
            attribute :>> criterion = "Stage 2: a frame retired while shared stays
                allocated after both releases and cannot be claimed; one retired
                inside a two-frame block stays allocated when the block is freed
                and its neighbour is free; one retired while free is allocated at
                once; a second retirement answers already. Negative control: the
                free paths' test removed, stage 2 stops. test-mce-driver: a pinned retired
                frame is still listed after quarantine_release; its keep is stage
                2's check (control: the keep removed, stage 2 stops)";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("mm::retire_frame",
                "mm::holds_retired",
                "mm::keep_retired",
                "mm::is_retired",
                "mm::release_frame",
                "mm::deallocate_frames",
                "mm::retired_frames");
        }
        requirement <'L.mm.92'> theRetiredSetRefusesWhenFull : ItemLowLevel {
            attribute :>> statement = "Once 256 frames are retired, retire_frame
                shall record no further frame and change nothing, answering full,
                and shall print the set's fullness once.";
            attribute :>> criterion = "Stage 2, the set's room lowered to what the
                check has used: a further frame is refused twice and not retired,
                and the line was printed; the room and the line are given back.";
            attribute :>> parent = "H.FAIL.8";
            attribute :>> unit = ("mm::retire_frame");
        }
        // docs/HEAP-CACHES.md: each processor's cache of free heap objects in
        // front of the heap's lock (the consultant's OK IF 1-10, ledger 4914).
        requirement <'L.mm.93'> anAllocationIsServedByItsProcessorsCache : ItemLowLevel {
            attribute :>> statement = "An allocation of a size class shall take an
                object its processor's cache holds without the heap's lock, and an
                empty cache shall refill REFILL objects of the class under one hold of
                it.";
            attribute :>> criterion = "On one processor with its 2 KiB cache drained,
                the first allocation raises the heap's count by exactly REFILL objects
                and the next REFILL - 1 do not move it.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::allocate_through_caches", "mm::with_cache");
        }
        requirement <'L.mm.94'> aFreeGoesToItsProcessorsCache : ItemLowLevel {
            attribute :>> statement = "A free of a size class shall go to its
                processor's cache while the cache has room; a full cache shall give
                back REFILL of its objects and the freed one under one hold of the
                heap's lock.";
            attribute :>> criterion = "REFILL frees into an empty 2 KiB cache leave
                the heap's count unchanged and the cache holding REFILL; with the
                cache full, one more free lowers the count by exactly REFILL + 1
                objects and leaves CACHE_DEPTH - REFILL held.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "mm::KernelAllocator::dealloc";
        }
        requirement <'L.mm.95'> aRefusalDrainsTheCachesFirst : ItemLowLevel {
            attribute :>> statement = "Every refusal of the heap -- a refill, a large
                request, a reserve's fill -- shall drain every processor's cache and
                try once more before it refuses; a reserved section then draws on its
                reserve as before.";
            attribute :>> criterion = "With every free frame held and this
                processor's cache holding 2 KiB objects made one after another, whole
                pages of them, an allocation of one page is served, and the cached
                bytes have fallen; so is a reserved section whose fill needs one 2 KiB
                object, every free one of that class taken from the heap, and one
                that holds a page, each with the cached bytes fallen and the reserve
                full after it.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::heap_or_drain", "mm::reserve::enter");
        }
        requirement <'L.mm.96'> theHeapCountLeavesTheCachesOut : ItemLowLevel {
            attribute :>> statement = "heap_allocated() and the held-memory window's
                heap bytes shall be the heap's allocated bytes less the bytes the
                processors' caches hold.";
            attribute :>> criterion = "With a 2 KiB cache full, heap_allocated() is
                the heap's own count less the cached bytes, and the cached bytes are
                at least CACHE_DEPTH objects.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "mm::heap_allocated";
        }
        requirement <'L.mm.97'> theDrainEmptiesEveryCache : ItemLowLevel {
            attribute :>> statement = "The drain shall give every processor's cached
                objects back to the heap, taking one cache lock at a time, and shall
                stop the machine (FX-0012) when entered with this processor's cache
                lock held.";
            attribute :>> criterion = "After a drain this processor's cache holds
                nothing and the heap's count is where it was before the objects were
                cached; every processor allocating, freeing across processors and
                draining at once finishes within 20 s with no allocation refused, every
                object they made freed and the caches empty after a drain.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "mm::drain_caches";
        }
        requirement <'L.mm.98'> anInjectedRefusalIsNotRetried : ItemLowLevel {
            attribute :>> statement = "A refusal the failure injection made shall be
                answered without a drain or a retry.";
            attribute :>> criterion = "With warm caches, an injection refusing the
                next frame makes the next allocation of one page return null, and the
                injection counts exactly one failure.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::heap_or_drain", "fallible::injected_failures");
        }
        // docs/RAM-RELEASE.md §2, R1 (consultant ledger 4971, 5022).
        requirement <'L.mm.99'> theArchivesPagesComeBack : ItemLowLevel {
            attribute :>> statement = "Once the root is final, at most once a boot, the
                frames the initramfs archive fills whole and the frame allocator
                describes shall be given to it, in batches of at most 1024 frames a
                hold, and MemTotal and the watermark shall be computed from the
                frames managed after.";
            attribute :>> criterion = "On every boot whose checks run, the frames
                managed grow by exactly the archive's whole pages inside the frame
                array, none of them is Reserved after, and the watermark is
                watermark_for of the new count, in one hold per aligned batch of
                1024 frames it touches and no more than 1024 frames a hold; and
                whole_frames counts an archive unaligned at both ends, an aligned
                one, one the frame array ends inside, one at frame 0 and one
                filling no page whole as inward and inside the array (the
                `release  checked` line). No release, the watermark left as it
                was, or the first page rounded down stops the boot there.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::give_back_initrd", "mm::initrd_frames",
                "mm::whole_frames");
        }
        requirement <'L.mm.100'> noReaderOutlivesTheArchive : ItemLowLevel {
            attribute :>> statement = "The initramfs archive shall be kept only when it
                lies in one Initrd region inside the direct map, read only under
                its lock, lent to a closure whose answer cannot borrow it, and
                give_back_initrd shall take it out under that lock before any of its
                frames is given away; after that it shall answer nothing.";
            attribute :>> criterion = "After the release with_initrd and initrd_span
                answer None on every boot whose checks run, and initrd_readable
                takes an archive unaligned at both ends inside an Initrd region and
                refuses one across the region's end, one in a region of another
                kind or in none, and one past the direct map (the `release
                checked` line). A release that leaves the archive kept, or a range
                test that takes every range, stops the boot there.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::keep_initrd", "mm::initrd_readable", "mm::with_initrd",
                "mm::give_back_initrd");
        }
        requirement <'L.mm.101'> aScrubClaimLeavesEveryOrder : ItemLowLevel {
            attribute :>> statement = "The scrubber shall claim a frame only in one hold of the frame
                allocator's lock in which the frame is free, not retired, free
                memory is at least twice the watermark, and another free block
                of the claimed block's order or larger exists; otherwise it
                shall skip the frame, and stop the batch at the watermark.";
            attribute :>> criterion = "ferrix-frame's host test
                a_claim_leaves_another_block_of_its_order; check_scrubber: a
                frame the check holds is skipped, its contents untouched, and
                a clean frame's batch leaves the free count and every order's
                free-block count as they were. Control: the claim's free test
                removed -- the held frame changes.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::claim", "mm::scrub::run", "mm::scrub::test_batch");
        }
        requirement <'L.mm.102'> aScrubbedFrameGoesBackUncounted : ItemLowLevel {
            attribute :>> statement = "A frame the scrubber found good shall be zeroed and freed
                through the path that keeps a retired frame held, moving none
                of the allocator's counters, as its claim moved none.";
            attribute :>> criterion = "check_scrubber: the free count and the free-block counts after
                a clean frame's batch equal those before.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::release", "mm::scrub::zero");
        }
        requirement <'L.mm.103'> fourPatternsEachReadBack : ItemLowLevel {
            attribute :>> statement = "Each test shall write the frame four times -- 0x5555..,
                0xAAAA.., each word its physical address, and that inverted --
                each with writes that read nothing, evict it to the point of
                coherency, and read every word back and compare; it shall read
                no line whose memory contents it has not itself written in the
                current pass.";
            attribute :>> criterion = "test-scrub: a full sweep of good memory finds nothing bad;
                check_scrubber's injected failure is seen on a read-back; the
                arch requirements L.x86_64.243, L.aarch64.68 and L.armv7a.29
                for the writes and the eviction.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::test", "mm::scrub::injected");
        }
        requirement <'L.mm.104'> aFrameIsBadOnlyAfterItsRetest : ItemLowLevel {
            attribute :>> statement = "A frame that fails a test shall be tested again at once; it is
                bad if the retest fails or if it failed before as one of the
                last 64 suspects; one that passes its retest shall be
                remembered as a suspect and released.";
            attribute :>> criterion = "check_scrubber: an injected one-off makes a suspect, released;
                the same frame failing again later is retired; an injected
                stuck bit is retired after its retest. Control: retirement
                after one failure -- the one-off retires a good frame.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::judge", "mm::scrub::was_suspect", "mm::scrub::remember_suspect");
        }
        requirement <'L.mm.105'> aBadFrameIsKeptAndTheShareHeld : ItemLowLevel {
            attribute :>> statement = "A bad frame shall be retired and freed through the
                retired-frame decision, so that it stays allocated, owned by
                nobody and retired; where the retired set is full or the
                scrubber's share of 192 used, it shall be held claimed, not
                released, and the batch shall stop, so that at most one frame
                is held out.";
            attribute :>> criterion = "check_scrubber: the retired frames are allocated, owned by
                nobody and in the set; with the set full a bad frame is held
                out and the batch stops after it. Control: a bad frame
                released when the set is full.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::keep_bad", "mm::scrub::share_used");
        }
        requirement <'L.mm.106'> theScrubberStandsDownWhereItMust : ItemLowLevel {
            attribute :>> statement = "At boot the kernel shall decide once, from SMBIOS type 16's
                system memory arrays, whether the scrubber runs: where every
                array says None; and where they say Other or Unknown or none
                exists only if the processor writes without reading; never
                where one says Parity, ECC or CRC or a type 16 cannot be read,
                nor as a guest (or where AArch64 firmware names no PSCI
                conduit); the gate's override, test-only and refused in a
                certified profile's command line, is honoured only where a
                hypervisor is positively seen (x86-64's hypervisor bit,
                AArch64's hvc conduit or /hypervisor node).";
            attribute :>> criterion = "ferrix-smbios's host test
                the_memory_arrays_say_whether_memory_corrects; test-scrub: the
                boot without the override stands down for QEMU's multi-bit
                ECC, the override taken under QEMU's hypervisor, ARMv7-A's
                stand-down. Control: the type-16 stand-down removed -- the
                boot without the override runs.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::decide", "mm::scrub::verdict", "mm::scrub::standing", "mm::scrub::override_asked", "mm::scrub::Standing::reason", "mm::scrub::Standing::from_number");
        }
        requirement <'L.mm.107'> theScrubbersCountsShowCoverage : ItemLowLevel {
            attribute :>> statement = "/proc/ferrix-scrub, readable by all, shall give the frames
                managed, the frames tested in all and this sweep, the sweeps,
                the suspects, bad, retired and held counts, and the standing
                with its reason, and no address.";
            attribute :>> criterion = "test-scrub: the guest reads tested against managed after a
                sweep and the standing in both boots.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::render");
        }
        requirement <'L.mm.108'> oneBatchAtATime : ItemLowLevel {
            attribute :>> statement = "Only one batch shall run at a time machine-wide, of at most 64
                frames; a batch asked for while one runs shall be refused, and
                a budget of 0 or past 64 refused, with nothing tested.";
            attribute :>> criterion = "The native boot check: a budget of 0 and of 65 refused
                INVALID_ARGS; test-scrub's memscrub batches answered.";
            attribute :>> parent = "H.FAIL.9";
            attribute :>> unit = ("mm::scrub::test_batch", "mm::scrub::batch_for_check");
        }
        // Restated by docs/RAM-RELEASE.md R1 (consultant ledger 5022, condition 1):
        // the archive's release sets it a second time.
        requirement <'L.mm.71'> theWatermarkIsSizedOnce : ItemLowLevel {
            attribute :>> statement = "init shall set the watermark to the frames the
                allocator manages divided by 128, and no fewer than 256 frames nor
                more than 8192; give_back_initrd shall set it once more, the same
                way, from the frames managed after the initramfs's release, in the
                hold of the frame allocator that inserts the release's last frames.";
            attribute :>> criterion = "The watermark equals watermark_for of the
                frames managed at init, and watermark_for gives 256 for 0 frames,
                300 for 38400 and 8192 for the largest count (the `watermark`
                line); after the release it equals watermark_for of the frames
                managed then (the `release` line).";
            attribute :>> parent = "H.QUOTA.13";
            attribute :>> unit = ("mm::init", "mm::watermark_for", "mm::watermark",
                "mm::give_back_initrd");
        }
        requirement <'L.mm.72'> aProgramsFramesStopAtTheWatermark : ItemLowLevel {
            attribute :>> statement = "allocate_user_frame and allocate_user_run
                shall refuse frames that would leave fewer than the watermark free,
                deciding in the same hold of the frame allocator's lock as the
                frames are taken, and a refusal shall take back any charge made for
                them.";
            attribute :>> criterion = "At stage 2, with no job charged, single
                frames taken until refused leave exactly the watermark free; a run
                of two is given at two above it and refused at one above it while a
                block of two is free; a single frame is given at one above it.";
            attribute :>> parent = "H.QUOTA.13";
            attribute :>> unit = ("mm::allocate_program_frames",
                "mm::allocate_user_frame", "mm::allocate_user_run");
        }
        requirement <'L.mm.73'> aProgramsTablesStopAtTheWatermark : ItemLowLevel {
            attribute :>> statement = "A page table for a user mapping shall be
                taken as a program's frame, refused at the watermark whether or not
                its job has a quota slot, and zeroed before it is used.";
            attribute :>> criterion = "At stage 2, with exactly the watermark free,
                a program's page table with no job to charge is refused; with one
                frame more it is given, and the free count is the watermark again.";
            attribute :>> parent = "H.QUOTA.13";
            attribute :>> unit = ("mm::ChargedTables::allocate_table", "mm::map_in");
        }
        requirement <'L.mm.74'> theKernelIsServedBelowTheWatermark : ItemLowLevel {
            attribute :>> statement = "allocate_frames and the heap's page supply
                shall not be refused by the watermark.";
            attribute :>> criterion = "At stage 2, with exactly the watermark free,
                a kernel frame is given, and then a 16 KiB heap allocation takes a
                fresh page; both are given back and the frame window closes on the
                count it opened at.";
            attribute :>> parent = "H.QUOTA.13";
            attribute :>> unit = ("mm::allocate_frames", "mm::KernelPages::allocate_pages");
        }
    }

    package AllocationReserve {
        doc /* src/kernel/src/mm/reserve.rs: each processor's allocation reserve, which
             * lets an Arc or a map insert that has started finish (finding F-23). */

        requirement <'L.mm.35'> aSectionCompletesOnTheReserve : ItemLowLevel {
            attribute :>> statement = "Inside a reserved section, an allocation the
                heap does not serve shall be served from the processor's reserve, a
                block of its size class or the large block held for it, so that the
                section's operation completes.";
            attribute :>> criterion = "With every allocation inside a section sent to
                the reserve without asking the heap, an Arc of 64 bytes, an Arc of 3000
                bytes (past the largest size class) and 200 map inserts each complete
                and hold what was put in them, and the reserve is drawn on at least 4
                times.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::reserve::reserve", "mm::reserve::enter",
                "mm::reserve::slot", "mm::reserve::draw", "mm::reserve::leave",
                "mm::reserve::Reserved::drop", "mm::KernelAllocator::alloc",
                "mm::reserve::bypassing_heap");
        }
        requirement <'L.mm.36'> aSectionThatCannotFillIsRefused : ItemLowLevel {
            attribute :>> statement = "reserve and fill_reserve shall refuse with
                AllocError, count the refusal and leave interrupts as they found them,
                when the heap cannot fill the processor's reserve or hold the large
                block asked for.";
            attribute :>> criterion = "With the heap unable to take a page and the
                reserve drawn below its depth, reserve is refused AllocError, the
                refused count rises by 1, and interrupts are enabled after as before;
                the same for a large block the heap cannot supply.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::reserve::reserve", "mm::reserve::enter",
                "mm::reserve::fill_reserve");
        }
        requirement <'L.mm.37'> theReserveServesOnlyItsSections : ItemLowLevel {
            attribute :>> statement = "draw shall serve an allocation from a reserve
                only while a section is open on that processor.";
            attribute :>> criterion = "With the heap unable to take a page, an
                allocation made outside any section is refused and the drawn count is
                unchanged, while the same allocation inside a section is served.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::reserve::draw", "mm::reserve::slot");
        }
        requirement <'L.mm.38'> aLargeBlockGoesBackWithTheSection : ItemLowLevel {
            attribute :>> statement = "When the outermost section on a processor
                closes, leave shall give a large block that section did not use back
                to the heap.";
            attribute :>> criterion = "A section entered for a large Arc and closed
                without making it leaves the heap's large pages and the free frame
                count where they were before the section.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("mm::reserve::leave", "mm::reserve::Reserved::drop");
        }
    }

    package UnlinkedTables {
        doc /* src/kernel/src/mm/unlinked.rs: page tables an unmap has unlinked from a
             * live tree, held from the allocator until the shootdown or the unit's
             * invalidation that covers them (finding F-36). */

        requirement <'L.mm.39'> theShootdownGivesBackTheHeldTables : ItemLowLevel {
            attribute :>> statement = "UnlinkedTables::release, called once the
                shootdown covering the unmap has returned, shall give every table on the
                list back to the frame allocator, counting a user tree's as user
                tables.";
            attribute :>> criterion = "The shootdown for an unmap_in that emptied
                tables leaves the list empty, and the user tables given back rise by
                exactly the number it held.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("mm::unlinked::UnlinkedTables::release",
                "mm::unlinked::UnlinkedTables::pop");
        }
        requirement <'L.mm.40'> mergedShootdownsKeepEveryTable : ItemLowLevel {
            attribute :>> statement = "take_from shall move every table of one list
                onto another, so that shootdowns merged into one release the tables of
                both, and none before it.";
            attribute :>> criterion = "Two lists of 2 and 3 tables merged with
                take_from leave the first empty and the second holding 5, all 5 still
                allocated; releasing the second gives back 5.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = ("mm::unlinked::UnlinkedTables::take_from",
                "mm::unlinked::UnlinkedTables::push",
                "mm::unlinked::UnlinkedTables::pop");
        }
        requirement <'L.mm.41'> aDroppedListKeepsItsTables : ItemLowLevel {
            attribute :>> statement = "A list dropped while it still holds tables
                shall keep them from the frame allocator for good and count them.";
            attribute :>> criterion = "A list holding 1 table, dropped unreleased,
                raises tables_kept by 1 and the table stays allocated: claim_frame
                refuses it.";
            attribute :>> parent = "H.MEM.7";
            attribute :>> unit = "mm::unlinked::UnlinkedTables::drop";
        }
    }

    package KernelArena {
        doc /* src/kernel/src/vmap.rs: the kernel's dynamic address space -- anonymous
             * allocations, device windows, buffers and guard-paged kernel stacks.
             * check_failed_device_map and check_invariants are check code in this
             * product file (mm-draft-A-notes.md). */

        requirement <'L.mm.42'> arenaAllocationsAreApart : ItemLowLevel {
            attribute :>> statement = "vmap::allocate shall hand out address ranges
                inside the arena, no two of them sharing an address or a guard page.";
            attribute :>> criterion = "Two live 8-page allocations have different
                bases, lie inside ARENA_BASE..ARENA_END, and the higher begins at least
                two pages past the end of the lower.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("vmap::init", "vmap::allocate", "vmap::reserve");
        }
        requirement <'L.mm.43'> arenaAllocationsAreGuarded : ItemLowLevel {
            attribute :>> statement = "vmap::allocate shall leave the page directly
                below and the page directly above every allocation unmapped.";
            attribute :>> criterion = "The page below an 8-page allocation and the page
                at its end do not translate.";
            attribute :>> parent = "H.FAIL.2";
            attribute :>> unit = ("vmap::allocate", "vmap::reserve");
        }
        requirement <'L.mm.44'> arenaPagesAreTheirOwnFrames : ItemLowLevel {
            attribute :>> statement = "vmap::allocate shall back each page of an
                allocation with a writable frame of its own.";
            attribute :>> criterion = "Each of the 8 pages of a fresh allocation
                translates, and a different value written to the first word of each is
                read back from every page after all 8 are written.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("vmap::allocate", "mm::map_kernel");
        }
        requirement <'L.mm.45'> arenaPagesAreZeroed : ItemLowLevel {
            attribute :>> statement = "vmap::allocate shall zero every byte of each
                page before mapping it.";
            attribute :>> criterion = "An 8-page allocation made after 8 frames were
                filled with a pattern and given back reads 0 in every byte of every
                page, 32768 of 32768.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = ("vmap::allocate", "mm::zero_frame");
        }
        requirement <'L.mm.46'> freeGivesTheAllocationBack : ItemLowLevel {
            attribute :>> statement = "vmap::free shall unmap an allocation and give
                all its frames back, and shall refuse an address it has already taken
                back.";
            attribute :>> criterion = "Two 8-page allocations are freed; a second free
                of the first is refused; its base no longer translates; and the free
                frame count is back where it stood after a warm-up pair had been made
                and freed.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("vmap::free", "vmap::claim", "vmap::release_span",
                "mm::unmap_kernel_all", "mm::deallocate_frames");
        }
        requirement <'L.mm.47'> anEmptyAllocationIsRefused : ItemLowLevel {
            attribute :>> statement = "vmap::allocate shall refuse an allocation of
                zero pages as a bad length.";
            attribute :>> criterion = "An allocation of 0 pages is refused
                BadLength(0).";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = ("vmap::allocate", "vmap::reserve");
        }
        requirement <'L.mm.48'> aFailedAllocationKeepsNoFrame : ItemLowLevel {
            attribute :>> statement = "vmap::allocate, allocate_stack and free_stacks
                shall answer an allocation refused part way as running out of memory,
                undoing what was done, and keep no frame of it.";
            attribute :>> criterion = "With each allocation that making three kernel
                stacks and freeing them -- one alone, then two in one call -- makes
                failed in turn, frames included, every run ends in success or as
                running out of memory, and the frame window alloc_check's run opens
                around the runs keeps 0 frames.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("vmap::allocate", "vmap::unwind", "vmap::release",
                "vmap::release_span", "vmap::allocate_stack", "vmap::free_stacks",
                "mm::allocate_frames");
        }
        requirement <'L.mm.49'> deviceWindowsLandWhereAsked : ItemLowLevel {
            attribute :>> statement = "map_device shall map a device window as device
                memory, not executable, at an address with the same offset in its page
                as the physical address asked for, and unmap_device shall take it down
                without giving the aperture's frames to the allocator.";
            attribute :>> criterion = "A window of 0x100 bytes at offset 0x40 into a
                frame starts at offset 0x40 in its page, translates to the address
                asked for and is mapped device and not executable; unmapped, it no
                longer translates and the free frame count is unchanged.";
            attribute :>> parent = ("H.MEM.15", "H.MEM.14");
            attribute :>> unit = ("vmap::map_device", "vmap::map_reserved",
                "vmap::unmap_device", "vmap::free", "mm::map_kernel");
        }
        requirement <'L.mm.50'> deviceWindowsRefuseTheImage : ItemLowLevel {
            attribute :>> statement = "map_device shall refuse, before it reserves or
                maps anything, a range that touches any byte of the kernel image.";
            attribute :>> criterion = "Windows asked for over the image's first 0x100
                bytes, a two-page range from the page below it into it, its last byte
                and a page at its middle are each refused KernelImage naming the
                address asked for, 4 of 4, and the arena holds as many allocations as
                before.";
            attribute :>> parent = "H.MEM.10";
            attribute :>> unit = ("vmap::map_device", "mm::overlaps_image",
                "mm::image_span_overlaps", "vmap::usage", "vmap::VmapError::fmt");
        }
        requirement <'L.mm.51'> deviceWindowsRefuseAWrappedRange : ItemLowLevel {
            attribute :>> statement = "map_device shall refuse as a bad length, before
                it reserves anything, a range whose end rounded out to a page is past
                the top of the address space.";
            attribute :>> criterion = "Four ranges that wrap -- the offset plus the
                length, the rounding of that, the top page with a page's length, and
                0x100 bytes at 0x40 into the top page -- are each refused BadLength
                naming the length asked for, 4 of 4, and the arena holds as many
                allocations as before.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = ("vmap::map_device", "vmap::usage");
        }
        requirement <'L.mm.52'> aFailedDeviceMapLeavesNothing : ItemLowLevel {
            attribute :>> statement = "A device window whose mapping fails part way
                shall leave none of its own pages mapped and remove no mapping it did
                not make.";
            attribute :>> criterion = "A two-page window whose second page is already
                mapped is refused, its first page does not translate afterwards, and
                the mapping in its way still translates to where it did.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = ("vmap::map_reserved", "vmap::unmap_installed");
        }
        requirement <'L.mm.53'> theArenaStaysConsistent : ItemLowLevel {
            attribute :>> statement = "The arena's regions shall stay sorted and
                non-overlapping, and every live allocation's span shall stay reserved
                in it, through every reserve, claim and release.";
            attribute :>> criterion = "After the stage 2 arena checks' allocations,
                windows and frees, the arena's regions are sorted and do not overlap,
                and each live allocation's span is found in it, 0 exceptions.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = ("vmap::reserve", "vmap::claim", "vmap::release",
                "vmap::release_span");
        }
        requirement <'L.mm.54'> deviceWindowsAreListed : ItemLowLevel {
            attribute :>> statement = "device_windows shall list the physical range of
                every live device window and nothing an anonymous allocation holds.";
            attribute :>> criterion = "With one device window over a known frame and
                one anonymous allocation live, device_windows lists the window's range
                and no range of the anonymous allocation's frames; with the window
                gone, it no longer lists it.";
            attribute :>> parent = "H.DMA.5";
            attribute :>> unit = "vmap::device_windows";
        }
        requirement <'L.mm.55'> kernelStacksAreGuarded : ItemLowLevel {
            attribute :>> statement = "allocate_stack shall make a kernel stack of
                STACK_PAGES pages, its top 16-byte aligned and every byte of it
                writable, with an unmapped page directly below and above it, and
                free_stack shall give all its frames back.";
            attribute :>> criterion = "A stack from allocate_stack is 4 pages long with
                its top 16-byte aligned; a value written at its first word and at its
                last is read back; the page below its base and the page at its top do
                not translate; and free_stack returns the free frame count to where it
                was.";
            attribute :>> parent = ("H.FAIL.2", "H.MEM.9");
            attribute :>> unit = ("vmap::allocate_stack", "vmap::free_stack");
        }
        requirement <'L.mm.56'> stacksAreFreedUnderOneShootdown : ItemLowLevel {
            attribute :>> statement = "free_stacks shall take every stack it is given
                out of use, unmap all of them under one shootdown, and make their
                addresses available again only after it.";
            attribute :>> criterion = "Freeing 8 stacks with free_stacks takes exactly
                1 shootdown, gives back every frame, and none of the 8 addresses is
                handed out again before the shootdown has returned.";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = "vmap::free_stacks";
        }
        requirement <'L.mm.57'> aBufferIsZeroedPages : ItemLowLevel {
            attribute :>> statement = "Buffer::zeroed shall make a buffer of len
                zeroed bytes on pages that need not be contiguous, none for a length of
                zero, and the buffer's drop shall free them.";
            attribute :>> criterion = "A buffer of 5 MiB, more than the heap's largest
                block, reads 0 in every byte and holds what is written; a buffer of 0
                bytes takes no allocation; with both dropped the arena's allocations
                and the free frame count are where they were.";
            attribute :>> parent = ("H.MEM.6", "H.MEM.9");
            attribute :>> unit = ("vmap::Buffer::zeroed", "vmap::Buffer::deref",
                "vmap::Buffer::deref_mut", "vmap::Buffer::drop",
                "vmap::Buffer::truncate");
        }
    }

    package EarlyMemory {
        doc /* src/kernel/src/early.rs: physical memory and page tables before there is
             * a frame allocator -- the loader's tables extended from a pool of
             * sixteen frames in .bss. read_physical_byte and translate serve only
             * stage 1's checks, which are in main.rs (mm-draft-A-notes.md). */

        requirement <'L.mm.58'> earlyWindowsTranslateAsAsked : ItemLowLevel {
            attribute :>> statement = "map_device and map_framebuffer shall install in
                the loader's tables a window that translates each page to the physical
                address asked for, with device or framebuffer attributes.";
            attribute :>> criterion = "The framebuffer window, where firmware left one
                outside reclaimable memory, translates to the framebuffer's physical
                address; a device window translates to the registers asked for and is
                mapped device and not executable.";
            attribute :>> parent = ("H.BOOT.1", "H.MEM.15");
            attribute :>> unit = ("early::EarlyMemory::new",
                "early::EarlyMemory::map_window", "early::EarlyMemory::map_device",
                "early::EarlyMemory::map_framebuffer",
                "early::EarlyMemory::read", "early::EarlyMemory::write");
        }
        requirement <'L.mm.59'> earlyWindowsRefuseTheImage : ItemLowLevel {
            attribute :>> statement = "map_window shall refuse, before it maps
                anything, a range that touches any byte of the kernel image.";
            attribute :>> criterion = "Early device windows asked for over the image's
                first page, a range straddling its start, its last byte and a range
                straddling its end are each refused KernelImage naming the address
                asked for, 4 of 4, and the window address still does not translate.";
            attribute :>> parent = "H.MEM.10";
            attribute :>> unit = ("early::EarlyMemory::map_window",
                "mm::image_span_overlaps");
        }
        requirement <'L.mm.60'> earlyWindowsRefuseAWrappedRange : ItemLowLevel {
            attribute :>> statement = "map_window shall refuse with RangeOverflow,
                before it judges the range against the image or maps anything, a range
                whose end rounded out to a page is past the top of the address
                space.";
            attribute :>> criterion = "The top page with a page's length and a page
                with a length of 2^64 - 1 are each refused MapFailed(RangeOverflow), 2
                of 2, and the window address still does not translate.";
            attribute :>> parent = "H.MEM.15";
            attribute :>> unit = "early::EarlyMemory::map_window";
        }
        requirement <'L.mm.61'> theEarlyTablePoolIsBounded : ItemLowLevel {
            attribute :>> statement = "EarlyMemory::allocate_table shall hand out each
                of its 16 pool frames at most once and then refuse, which a mapping
                reports as running out of memory.";
            attribute :>> criterion = "An EarlyMemory asked for 17 tables answers 16
                distinct page-aligned frames inside the pool and then none; a mapping
                that needs a table after that is refused MapFailed(OutOfMemory).";
            attribute :>> parent = "H.MEM.14";
            attribute :>> unit = "early::EarlyMemory::allocate_table";
        }
        requirement <'L.mm.62'> onlyRamHasACheckedDirectMapAlias : ItemLowLevel {
            attribute :>> statement = "direct_map_ram shall answer a direct-map address
                only for a physical address inside a run of RAM that record_ram
                recorded from the memory map, clipped to the direct map's span, and
                record_ram shall record those runs whole, in whole pages, ascending
                and apart, or refuse to bring memory up.";
            attribute :>> criterion = "At least one run is recorded, each whole pages,
                ascending and apart; a frame the allocator hands out is answered with
                the direct_map address; the page past the highest run, a page between
                two runs where the memory map has one, and a device's aperture are
                each answered None (the stage 2 memory check and the `copies` line).";
            attribute :>> parent = ("H.MEM.1", "H.MEM.15");
            attribute :>> unit = ("mm::direct_map_ram", "mm::record_ram");
        }
        requirement <'L.mm.63'> aLibraryMapInsertRunsInASection : ItemLowLevel {
            attribute :>> statement = "A library crate's map insert through
                ferrix_fallible shall run in a reserved section the kernel installs at
                boot: completed from the reserve when the heap refuses, refused before
                it starts when the reserve cannot be filled, and refused when its nodes
                are larger than any size class.";
            attribute :>> criterion = "With every allocation inside a section sent to the
                reserve, 200 inserts through ferrix_fallible::try_map_insert complete,
                hold what was inserted and draw on the reserve at least twice; with the
                checking processor's fills refused, an insert is refused, the refused
                count rises and the map is unchanged; a map of 256-byte values is
                refused.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("fallible::library_section",
                "fallible::install_library_sections");
        }
        requirement <'L.mm.64'> refusedFillsStayOnOneProcessor : ItemLowLevel {
            attribute :>> statement = "refuse_reserve_fills shall make every section
                entered on the calling processor refused, as if its reserve could not be
                filled, until it is turned off.";
            attribute :>> criterion = "With fills refused, a library map insert on the
                checking processor is refused and the refused count rises; turned off
                again, the next insert completes.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("mm::reserve::refuse_reserve_fills",
                "mm::reserve::this_cpu_tag");
        }
    }

    package VirtualMemoryObjects {
        doc /* src/kernel/src/user/vmo.rs: the pageable memory object -- a sparse
             * list of frames committed on first touch, shared by a fork until
             * a write copies, held in place for a device, moved by mremap, cut
             * by a file's truncation -- and the three phases a page leaves it
             * by: out of the list, forgotten by every space that maps it and
             * shot down, and only then released. */

        requirement <'L.user.1'> aReservationCostsNothing : ItemLowLevel {
            attribute :>> statement = "Vmo::new_anonymous shall make an object of
                the pages asked for with no page committed and no frame taken.";
            attribute :>> criterion = "A fresh object of 1024 pages has 0 pages
                committed, the free-frame count is what it was before it was made,
                and its size reads 1024 pages and 1024 x 4096 bytes.";
            attribute :>> parent = "H.QUOTA.2";
            attribute :>> unit = ("user::vmo::Vmo::new_anonymous",
                "user::vmo::Vmo::unfilled", "user::vmo::Vmo::committed",
                "user::vmo::Vmo::len_pages", "user::vmo::Vmo::len_bytes");
        }
        requirement <'L.user.2'> aCommittedPageIsZeroed : ItemLowLevel {
            attribute :>> statement = "Vmo::commit shall give a page its first
                frame filled with zeros.";
            attribute :>> criterion = "Page 2 of a fresh 4-page object, committed,
                reads 0 in each of its 4096 bytes through the direct map.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = "user::vmo::Vmo::commit";
        }
        requirement <'L.user.3'> aFirstWriteCommitsAZeroedPage : ItemLowLevel {
            attribute :>> statement = "Vmo::write_page to a page that holds no
                frame shall commit a zeroed frame before it writes, so that the
                bytes of the page it does not write read as 0.";
            attribute :>> criterion = "A frame filled with a pattern and freed is
                taken by a write of 8 bytes at offset 100 of an uncommitted page;
                the other 4088 bytes of the page read 0.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = ("user::vmo::Vmo::write_page",
                "user::vmo::Pages::exclusive");
        }
        requirement <'L.user.4'> commitIsIdempotent : ItemLowLevel {
            attribute :>> statement = "Vmo::commit of a page that holds a frame
                shall hand back that frame and take no other.";
            attribute :>> criterion = "Page 3 committed twice gives the same frame
                both times, the second commit taking 0 frames; the object names that
                frame at page 3 and none at the untouched page 4.";
            attribute :>> parent = "H.QUOTA.2";
            attribute :>> unit = ("user::vmo::Vmo::commit", "user::vmo::Vmo::page");
        }
        requirement <'L.user.5'> commitRefusesPastTheEnd : ItemLowLevel {
            attribute :>> statement = "Vmo::commit shall refuse a page at or past
                the object's end as OutOfRange, naming the page and the object's
                size, and take no frame for it.";
            attribute :>> criterion = "commit of page 4 of a 4-page object is
                refused OutOfRange { index: 4, pages: 4 }, and the object has 0
                pages committed after.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::vmo::Vmo::commit", "user::vmo::VmoError::fmt");
        }
        requirement <'L.user.6'> commitWithinRefusesPastTheEnd : ItemLowLevel {
            attribute :>> statement = "Vmo::commit_within shall refuse a page at or
                past the object's end as OutOfRange, naming the page and the
                object's size.";
            attribute :>> criterion = "commit_within of page 1 of a 1-page object
                is refused OutOfRange { index: 1, pages: 1 }.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "user::vmo::Vmo::commit_within";
        }
        requirement <'L.user.7'> onlyWhatIsTouchedIsPaidFor : ItemLowLevel {
            attribute :>> statement = "Vmo::commit shall take one frame for each
                distinct page committed and no more, and dropping the object shall
                give every one of them back.";
            attribute :>> criterion = "Of a 2048-page object, 7 scattered pages
                committed out of order leave exactly 7 committed and the free-frame
                count 7 lower; with the object dropped the count is what it was
                before it was made.";
            attribute :>> parent = ("H.QUOTA.2", "H.MEM.9");
            attribute :>> unit = ("user::vmo::Vmo::commit", "user::vmo::Vmo::committed",
                "user::vmo::Vmo::drop");
        }
        requirement <'L.user.8'> aSharedFrameOutlivesOneHolder : ItemLowLevel {
            attribute :>> statement = "Dropping an object shall give back its own
                reference on each frame it holds, and so free only the frames no
                other holder still references.";
            attribute :>> criterion = "A committed page given a second reference:
                with the object dropped the frame has 1 reference and is not free
                (the free count 1 below where it began); with the last reference
                released the count is back where it began.";
            attribute :>> parent = ("H.MEM.9", "H.MEM.8");
            attribute :>> unit = "user::vmo::Vmo::drop";
        }
        requirement <'L.user.9'> aVmoIsCountedAsAnObject : ItemLowLevel {
            attribute :>> statement = "Vmo::new_anonymous and Vmo::fork shall each
                charge one object to the running task's job for as long as the
                object lives, and be refused, making nothing, when the job's object
                limit is full; Vmo::new_filled, a file's page cache, shall charge
                none.";
            attribute :>> criterion = "In a job limited to 5 objects and holding 5,
                vmo_create is refused NO_MEMORY and a fork of an object is refused,
                0 objects counted beyond the limit; with every object dropped the
                job holds 0; a tmpfs file made in the job adds 0 objects.";
            attribute :>> parent = ("H.QUOTA.9", "H.QUOTA.4");
            attribute :>> unit = ("user::vmo::Vmo::new_anonymous",
                "user::vmo::Vmo::fork", "user::vmo::Vmo::new_filled");
        }
        requirement <'L.user.10'> anUncommittedPageReadsAsZeros : ItemLowLevel {
            attribute :>> statement = "Vmo::read_page of a page that holds no frame
                shall read zeros and commit nothing.";
            attribute :>> criterion = "A read of 4096 bytes of page 5 of a fresh
                8-page object returns 4096 zeros, and the object has 0 pages
                committed and the free-frame count is unchanged after.";
            attribute :>> parent = ("H.MEM.6", "H.QUOTA.2");
            attribute :>> unit = "user::vmo::Vmo::read_page";
        }
        requirement <'L.user.11'> aByteRangeLeavingThePageIsRefused : ItemLowLevel {
            attribute :>> statement = "Vmo::read_page and Vmo::write_page shall
                refuse, as OutOfRange and copying nothing, a page past the object's
                end or a byte range that runs past the end of its page.";
            attribute :>> criterion = "Of a 2-page object, a read and a write of
                page 2, and of 8 bytes at offset 4092 of page 0, and of a range
                whose end overflows, are each refused OutOfRange, 6 of 6, with 0
                pages committed and the caller's buffer unchanged.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::vmo::Vmo::check_span",
                "user::vmo::Vmo::read_page", "user::vmo::Vmo::write_page");
        }
        requirement <'L.user.12'> forkSharesAndAWriteCopies : ItemLowLevel {
            attribute :>> statement = "Vmo::fork shall give the new object each
                committed page by a further reference to the same frame, and
                Vmo::write_page to a page another object shares shall copy it
                first, so that the write is not seen through the other object.";
            attribute :>> criterion = "A 1-page object written `before` and forked
                names the same frame as its fork; `after!` written through the
                original moves it to another frame, and the original reads
                `after!` while the fork reads `before`.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::vmo::Vmo::fork", "user::vmo::Vmo::write_page",
                "user::vmo::Pages::exclusive", "user::vmo::Vmo::read_page");
        }
        requirement <'L.user.13'> aDisplacedSharedFrameIsTakenDown : ItemLowLevel {
            attribute :>> statement = "When Vmo::write_page copies a shared page, it
                shall have every address space that maps the object forget the
                page, invalidated on every processor that may hold it, before it
                gives back the object's reference on the original, so that a
                mapping faults the copy in.";
            attribute :>> criterion = "A shared object a process maps and reads on
                another processor, whose page a fork of the object shares, written
                through Vmo::write_page: the process then reads the bytes written,
                not the original's, and its space translates the page to the
                copy.";
            attribute :>> parent = ("H.MEM.17", "H.MEM.8");
            attribute :>> unit = ("user::vmo::Vmo::write_page", "user::vmo::Vmo::retire",
                "user::vmo::Vmo::retire_runs");
        }
        requirement <'L.user.14'> aReplaceGivesBackTheOldReference : ItemLowLevel {
            attribute :>> statement = "Vmo::replace shall put the new frame in the
                page, return the frame it displaced, and give back only the
                object's own reference on that frame.";
            attribute :>> criterion = "Replacing a page whose frame has a second
                holder returns that frame, the object names the new frame, and the
                old frame has 1 reference; with the other holder's release and the
                object dropped the free-frame count is back where it began, 0
                frames kept.";
            attribute :>> parent = ("H.MEM.8", "H.MEM.9");
            attribute :>> unit = ("user::vmo::Vmo::replace", "user::vmo::Vmo::take_page",
                "user::vmo::Retired::with_room", "user::vmo::Retired::frame",
                "user::vmo::Vmo::retire", "user::vmo::Vmo::retire_runs",
                "user::vmo::Vmo::drop");
        }
        requirement <'L.user.15'> aReplaceOfAnAbsentPageDisplacesNothing : ItemLowLevel {
            attribute :>> statement = "Vmo::replace of a page that holds no frame
                shall put the frame in and report nothing displaced.";
            attribute :>> criterion = "A replace of page 1 of a 2-page object with
                nothing committed returns nothing, and the object then names the
                frame given at page 1.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::vmo::Vmo::replace", "user::vmo::Vmo::take_page");
        }
        requirement <'L.user.16'> decommitReachesEveryMapper : ItemLowLevel {
            attribute :>> statement = "Vmo::decommit_range shall take a page out of
                every address space that maps the object, invalidated on every
                processor that may hold its translation, before the page's frame
                goes back to the allocator.";
            attribute :>> criterion = "On two or more processors, a page of a
                shared object that two processes on two processors have touched
                (rmap_check::check's setup) is decommitted: 1 page given back,
                neither space translates it, the kernel claims the freed frame and
                poisons it, and the process on the other processor then reads 0,
                not the poison, and writes to a new frame, the freed one keeping its
                poison.";
            attribute :>> parent = ("H.MEM.17", "H.MEM.6");
            attribute :>> unit = ("user::vmo::Vmo::decommit_range",
                "user::vmo::Vmo::take_range", "user::vmo::Pages::take_into",
                "user::vmo::Vmo::retire", "user::vmo::Vmo::retire_runs",
                "user::vmo::runs_of", "user::vmo::Vmo::mapped_by",
                "user::vmo::forget_together");
        }
        requirement <'L.user.17'> replaceReachesEveryMapper : ItemLowLevel {
            attribute :>> statement = "Vmo::replace shall take the page out of every
                address space that maps the object, invalidated on every processor
                that may hold its translation, before it gives back the displaced
                frame, so that every mapping reaches the new frame.";
            attribute :>> criterion = "On two or more processors, a page of a
                shared object two processes on two processors have touched
                (rmap_check::check's setup) is replaced with a frame holding a
                pattern: the displaced frame is returned, neither space translates
                the page, the kernel poisons the displaced frame, and the process on
                the other processor reads the pattern, not the poison, and writes
                into the new frame, as the other process then does.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::vmo::Vmo::replace", "user::vmo::Vmo::take_page",
                "user::vmo::Retired::frame", "user::vmo::Vmo::retire",
                "user::vmo::Vmo::retire_runs", "user::vmo::runs_of",
                "user::vmo::Vmo::mapped_by", "user::vmo::forget_together");
        }
        requirement <'L.user.18'> retireFoldsTheCallersShootdown : ItemLowLevel {
            attribute :>> statement = "Vmo::retire given the address space under
                whose lock phase one ran shall not ask that space again, shall run
                that space's pending shootdown together with the others', and shall
                release the frames only after every one has completed.";
            attribute :>> criterion = "A copy-on-write fault taken on one processor
                on a page the same space's thread has cached on another, finished
                through Vmo::take_page and Vmo::retire with its own space: the
                displaced frame, claimed back and poisoned once released, is never
                read by the other processor's thread, which reads the copy.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::vmo::Vmo::retire", "user::vmo::Vmo::retire_runs",
                "user::vmo::forget_together", "user::vmo::own_shootdown");
        }
        requirement <'L.user.19'> aDecommitNeedsNoMemory : ItemLowLevel {
            attribute :>> statement = "Vmo::decommit_range shall give back every page
                of its range that is not held, taken out of every space that maps
                the object, even when no allocation can be made: a chunk of pages
                at a time from the stack, and the spaces asked one at a time.";
            attribute :>> criterion = "80 pages of a mapped 160-page object,
                committed through faults, decommitted with every fallible
                allocation failing (at least 1 failed): 80 pages given back, 0 left
                committed, 0 still translated, and 0 frames kept.";
            attribute :>> parent = ("H.MEM.11", "H.MEM.9");
            attribute :>> unit = ("user::vmo::Vmo::decommit_range",
                "user::vmo::Vmo::decommit_in_chunks", "user::vmo::Pages::take_into",
                "user::vmo::runs_of", "user::vmo::Vmo::retire_runs",
                "user::vmo::Vmo::forget_one_at_a_time", "user::vmo::Vmo::next_mapper",
                "user::vmo::own_shootdown");
        }
        requirement <'L.user.20'> anAllocationFailureKeepsNothing : ItemLowLevel {
            attribute :>> statement = "Each operation of an object that allocates --
                commit, fork, write_page, hold, take_page, take_range, adopt_pages,
                attach, take_mapped_writes and a retirement's list of the spaces to
                ask -- shall answer a failed allocation as running out of memory, or
                carry on without the memory, and keep no frame.";
            attribute :>> criterion = "In the memory sweep, with each allocation of
                each scenario (fork, move, shared, split, cut, object, stacks, code)
                failed in turn, every run ends in success or in running out of
                memory, the run in which nothing failed succeeds, and each
                scenario's frame window keeps 0 frames (the `sweep` line).";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("user::vmo::Vmo::commit", "user::vmo::Vmo::fork",
                "user::vmo::Vmo::write_page", "user::vmo::Vmo::hold",
                "user::vmo::Pages::count_held", "user::vmo::Vmo::take_page",
                "user::vmo::Vmo::take_range", "user::vmo::Vmo::adopt_pages",
                "user::vmo::Vmo::attach", "user::vmo::compact",
                "user::vmo::Vmo::take_mapped_writes", "user::vmo::Retired::with_room",
                "user::vmo::Retired::has_room", "user::vmo::Retired::record",
                "user::vmo::Vmo::mapped_by", "user::vmo::Vmo::forget_one_at_a_time");
        }
        requirement <'L.user.21'> holdingCopiesASharedPage : ItemLowLevel {
            attribute :>> statement = "Vmo::hold shall commit each page of its
                range, copying into a frame of its own a page a fork left shared,
                with its contents, and report the frames in page order.";
            attribute :>> criterion = "Holding pages 1 and 2 of a 4-page object
                whose page 1 a fork shares reports 2 frames, the ones the object
                names at pages 1 and 2; page 1's is not the shared frame and has 1
                reference, the fork still names the shared one, and page 1 still
                reads `held`.";
            attribute :>> parent = ("H.DMA.3", "H.MEM.8");
            attribute :>> unit = ("user::vmo::Vmo::hold", "user::vmo::Pages::unshared",
                "user::vmo::Pages::exclusive", "user::vmo::Pages::count_held",
                "user::vmo::Retired::with_room", "user::vmo::Vmo::retire",
                "user::vmo::Held::frames", "user::vmo::Vmo::commit_run",
                "user::vmo::Pages::insert_run");
        }
        requirement <'L.user.22'> aHeldPageKeepsItsFrame : ItemLowLevel {
            attribute :>> statement = "While a page is held, Vmo::decommit_range and
                Vmo::decommit_from shall skip it, Vmo::replace shall refuse it and
                leave the frame offered with the caller, and Vmo::write_page shall
                write it where it is.";
            attribute :>> criterion = "With pages 1 and 2 of a 4-page object held,
                decommit_range(0, 4) and decommit_from(0) each give back 0 pages and
                2 stay committed; a replace of page 1 returns nothing, page 1 keeps
                the held frame, and the frame offered is freed by the caller's
                release; a write to page 1 leaves it at the held frame.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("user::vmo::Vmo::decommit_range",
                "user::vmo::Vmo::decommit_from", "user::vmo::Vmo::take_range",
                "user::vmo::Pages::take_into", "user::vmo::Vmo::replace",
                "user::vmo::Vmo::take_page", "user::vmo::Vmo::write_page",
                "user::vmo::Vmo::give_back_run", "user::vmo::Pages::take_ours");
        }
        requirement <'L.user.23'> aHeldPageStaysMappedEverywhere : ItemLowLevel {
            attribute :>> statement = "While a page is held, Vmo::decommit_range and
                Vmo::replace shall leave it mapped at its frame in every address
                space that maps the object.";
            attribute :>> criterion = "On two or more processors, a page of a
                shared object two processes map (rmap_check::check's setup) is
                held: a write by one process reaches the held frame; decommit_range
                gives back 0 pages and a replace is refused, the object still naming
                the held frame; both spaces translate the page to the held frame
                before and after, and both processes read the marker written.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("user::vmo::Vmo::hold",
                "user::vmo::Vmo::decommit_range", "user::vmo::Vmo::take_range",
                "user::vmo::Pages::take_into", "user::vmo::Vmo::replace",
                "user::vmo::Vmo::take_page");
        }
        requirement <'L.user.24'> forkCopiesAHeldPage : ItemLowLevel {
            attribute :>> statement = "Vmo::fork shall give the new object a copy of
                a held page, with its contents, rather than a reference to the held
                frame.";
            attribute :>> criterion = "A fork of an object whose page 1 is held
                names another frame than the held one at page 1, the held frame
                keeps 1 reference, and the fork's page 1 reads `held`.";
            attribute :>> parent = ("H.DMA.3", "H.MEM.8");
            attribute :>> unit = "user::vmo::Vmo::fork";
        }
        requirement <'L.user.25'> holdsNest : ItemLowLevel {
            attribute :>> statement = "A page shall stay held until the last Held
                covering it is dropped, and then be an ordinary page again.";
            attribute :>> criterion = "Page 2 held by two holds and page 1 by one:
                with the first hold dropped, decommit_range(0, 4) gives back 1 page
                and page 2 keeps its frame; with the second dropped it gives back 1
                page and 0 stay committed.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("user::vmo::Vmo::hold", "user::vmo::Held::drop",
                "user::vmo::Pages::count_held", "user::vmo::Pages::uncount_held");
        }
        requirement <'L.user.26'> aHoldOutsideTheObjectIsRefused : ItemLowLevel {
            attribute :>> statement = "Vmo::hold shall refuse a range of no pages
                and a range that runs past the object's end.";
            attribute :>> criterion = "Of a 4-page object, hold of 2 pages from
                page 3 and hold of 0 pages from page 0 are both refused.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "user::vmo::Vmo::hold";
        }
        requirement <'L.user.27'> heldPagesComeBack : ItemLowLevel {
            attribute :>> statement = "Holding pages, copying them for a hold or a
                fork, and letting the holds go shall keep no frame once the objects
                and their holds are dropped.";
            attribute :>> criterion = "The free-frame count taken before and after
                the held-page scenario (a write, a fork, two holds, a refused
                replace, a fork while held, decommits) is the same: 0 frames
                kept.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("user::vmo::Held::drop", "user::vmo::Vmo::drop",
                "user::vmo::Vmo::retire", "user::vmo::Vmo::retire_runs");
        }
        requirement <'L.user.28'> aHeldRangeIsReported : ItemLowLevel {
            attribute :>> statement = "Vmo::holds_any shall say whether any page of
                a range is held.";
            attribute :>> criterion = "Of a 4-page object with page 2 held,
                holds_any is true for pages 0 to 3 and for page 2 alone, and false
                for pages 0 and 1 and for page 3; with the hold dropped, false for
                pages 0 to 3.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "user::vmo::Vmo::holds_any";
        }
        requirement <'L.user.29'> aHeldPageDoesNotMove : ItemLowLevel {
            attribute :>> statement = "Vmo::adopt_pages shall refuse to move a range
                any page of which is held.";
            attribute :>> criterion = "An mremap that must move a private region,
                one of whose pages is held, is refused (SpaceError::OutOfMemory), 1
                refusal.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = "user::vmo::Vmo::adopt_pages";
        }
        requirement <'L.user.30'> aMoveTakesThePagesWithIt : ItemLowLevel {
            attribute :>> statement = "Vmo::adopt_pages shall name each committed
                page of the range in the new object at the same offset from the
                range's start, and Vmo::release_moved shall take them out of the old
                object, so that a private region an mremap moves reaches the pages
                it had.";
            attribute :>> criterion = "A two-page private mapping, written across
                its page boundary, grown to four pages by an mremap that must move
                it: at the new address its first page and the string across the
                boundary read as written and the old address reads nothing; shrunk
                to one page and moved to a fixed address, it still reads its first
                page as written.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::vmo::Vmo::adopt_pages",
                "user::vmo::Vmo::release_moved");
        }
        requirement <'L.user.31'> aFailedMoveIsUndone : ItemLowLevel {
            attribute :>> statement = "Vmo::disown_pages shall make the object the
                pages moved to forget every one without releasing a frame, and
                Vmo::adopt_pages refused for memory shall leave that object naming
                none, so that the object the pages came from still holds each once.";
            attribute :>> criterion = "An mremap of a written private region made to
                fail after its pages moved leaves the region reading what was
                written, each frame with the reference count it had, and 0 frames
                kept once the space is dropped.";
            attribute :>> parent = ("H.MEM.11", "H.MEM.9");
            attribute :>> unit = ("user::vmo::Vmo::disown_pages",
                "user::vmo::Vmo::adopt_pages");
        }
        requirement <'L.user.32'> theMappersAreCounted : ItemLowLevel {
            attribute :>> statement = "Vmo::attach shall record each address space
                that names the object, so that Vmo::mapper_count counts every live
                space that maps it.";
            attribute :>> criterion = "A shared object mapped by a process, and by
                the child forked from it, has a mapper count of 2 before either
                runs.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::vmo::Vmo::attach",
                "user::vmo::Vmo::mapper_count");
        }
        requirement <'L.user.33'> goneMappersArePruned : ItemLowLevel {
            attribute :>> statement = "Vmo::detach shall remove the one name it is
                given, the list shall drop spaces that have gone whenever it is
                taken, and it shall give back most of its room once it lists under a
                quarter of it.";
            attribute :>> criterion = "An object mapped 1000 times by one space
                under 1000 ids and 1 time by a second: with 999 ids unmapped and
                the second space dropped, mapper_count is 1, and the heap the list
                held is back within 4 entries' room of 1 mapper.";
            attribute :>> parent = ("H.MEM.17", "H.QUOTA.7");
            attribute :>> unit = ("user::vmo::Vmo::detach", "user::vmo::Vmo::attach",
                "user::vmo::compact");
        }
        requirement <'L.user.34'> aPrivateObjectHasOneMapper : ItemLowLevel {
            attribute :>> statement = "Vmo::attach shall stop the kernel with the
                PRIVATE_OBJECT_SHARED report rather than give an object that backs
                a private region a second mapper, or a private mapper to an object
                already mapped.";
            attribute :>> criterion = "A negative control, attaching a second
                mapper to an object with a private one, ends the boot with the
                PRIVATE_OBJECT_SHARED report; no mapping the kernel makes in a
                boot does so.";
            attribute :>> parent = ("H.MEM.8", "H.FAIL.1");
            attribute :>> unit = "user::vmo::Vmo::attach";
        }
        requirement <'L.user.35'> aMapperIsCharged : ItemLowLevel {
            attribute :>> statement = "Vmo::attach shall charge the heap one name of
                an object holds against the running job's memory limit, record
                nothing when the charge is refused, and the charge shall go with
                the name.";
            attribute :>> criterion = "In a job with a 32 KiB limit, shared mappings
                of one file are refused ENOMEM before the limit is passed while a
                sibling job maps one; with them unmapped every byte charged has come
                back.";
            attribute :>> parent = "H.QUOTA.7";
            attribute :>> unit = "user::vmo::Vmo::attach";
        }
        requirement <'L.user.36'> coherentOnlyWhileUnmapped : ItemLowLevel {
            attribute :>> statement = "Vmo::make_coherent shall be refused while any
                address space maps the object, and once none does shall mark it so
                that Vmo::is_coherent says so.";
            attribute :>> criterion = "A 1-page object mapped shared in a space is
                refused make_coherent; unmapped, make_coherent succeeds and
                is_coherent is true.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("user::vmo::Vmo::make_coherent",
                "user::vmo::Vmo::is_coherent", "user::vmo::Vmo::detach");
        }
        requirement <'L.user.37'> aFaultFillsFromTheFile : ItemLowLevel {
            attribute :>> statement = "Vmo::fill_for_fault shall have an object made
                over a file's source fill an absent page from the file before a
                fault commits it, the pages filled going in through
                Vmo::insert_absent, so that a mapping shows the file's bytes, never
                zeros in their place.";
            attribute :>> criterion = "Through a 64-page object over a page source
                mapped shared and privately (check_a_mapping_faults_in_its_source's
                setup), a read of page 5 through the shared mapping returns the
                source's byte after 1 call to the source, with 32 pages committed; a
                private write to page 50, which nothing had read, reads back beside
                the source's next byte, and the object's own page 50 still holds the
                source's bytes.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::vmo::Vmo::fill_for_fault",
                "user::vmo::Vmo::new_filled", "user::vmo::Vmo::insert_absent",
                "user::vmo::Vmo::fill_one_page");
        }
        requirement <'L.user.38'> insertAbsentKeepsWhatIsThere : ItemLowLevel {
            attribute :>> statement = "Vmo::insert_absent shall put a frame in a page
                only if the page is inside the object and holds nothing, and
                otherwise, or with no memory to record it, leave the frame with the
                caller.";
            attribute :>> criterion = "Of a 2-page object with page 0 committed,
                insert_absent of page 0, of page 2 and of page 1 with every
                allocation failing each answers false and page 0 keeps its frame; of
                page 1 it answers true and the object names the frame there; every
                frame refused is freed by the caller's release, 0 frames kept.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "user::vmo::Vmo::insert_absent";
        }
        requirement <'L.user.39'> pagesPastTheFileEndAreRefused : ItemLowLevel {
            attribute :>> statement = "Once Vmo::set_file_len has lowered the end of
                the file an object holds, a fault through a shared or a private
                mapping of it shall commit and reach no page wholly past that end,
                a page a private mapping copied included.";
            attribute :>> criterion = "A tmpfs file mapped shared and privately over
                4 pages, the private mapping having copied pages 0 and 1
                (check_a_shared_file_mapping's earlier steps), truncated to one
                page: page 0 still reads through the shared mapping, and each of the
                other 3 pages is refused through the shared mapping and through the
                private one, 3 of 3 each.";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = ("user::vmo::Vmo::set_file_len",
                "user::vmo::Vmo::past_file_end", "user::vmo::Vmo::commit_within",
                "user::vmo::Vmo::decommit_from", "user::vmo::Vmo::cut_mappings");
        }
        requirement <'L.user.40'> aCutTakesThePrivateCopies : ItemLowLevel {
            attribute :>> statement = "Vmo::cut_mappings shall take every mapping of
                the object's pages from a cut down, and give back the copies private
                mappings made of them once no processor can reach them, so that a
                file grown again shows zeros there through every mapping.";
            attribute :>> criterion = "The file of L.user.39's criterion grown back
                to two pages reads 4096 zero bytes at page 1 through the shared
                mapping and through the private mapping that had copied it.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = ("user::vmo::Vmo::cut_mappings",
                "user::vmo::Vmo::retire_runs", "user::vmo::forget_together",
                "user::vmo::Vmo::forget_one_at_a_time",
                "user::vmo::ShadowCopies::finish", "user::vmo::Vmo::decommit_from",
                "user::vmo::Vmo::commit_within");
        }
        requirement <'L.user.41'> aTruncationTakesTheFilesPages : ItemLowLevel {
            attribute :>> statement = "Vmo::decommit_from shall give back every
                committed page from its first page on that is not held, so that a
                file truncated and grown again reads zeros there, never its old
                bytes.";
            attribute :>> criterion = "A tmpfs file of 3 pages and 123 bytes,
                truncated into its second page and grown back, reads its first page
                and 10 bytes as written and 0 in every byte after.";
            attribute :>> parent = "H.MEM.6";
            attribute :>> unit = ("user::vmo::Vmo::decommit_from",
                "user::vmo::Vmo::decommit_range", "user::vmo::Vmo::take_range",
                "user::vmo::Pages::take_into", "user::vmo::Vmo::read_page");
        }
        requirement <'L.user.42'> theWriteSealCountsWritableMappings : ItemLowLevel {
            attribute :>> statement = "Vmo::raise_shared_may_write and
                Vmo::lower_shared_may_write shall count the shared mappings that may
                write the file an object holds, a fork child's copies included, so
                that Vmo::writably_mapped says whether any is left.";
            attribute :>> criterion = "F_SEAL_WRITE on a memfd is refused EBUSY
                while a writable shared mapping of it lives, refused EBUSY again
                while only a fork child's copy of that mapping does, and accepted
                once neither does.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::vmo::Vmo::raise_shared_may_write",
                "user::vmo::Vmo::lower_shared_may_write",
                "user::vmo::Vmo::writably_mapped");
        }
        requirement <'L.user.43'> mappedWritesAreReported : ItemLowLevel {
            attribute :>> statement = "Vmo::take_mapped_writes shall report every
                page the object holds while a shared mapping that may write has
                been made since the last report or still lives, none otherwise, and
                on running out of memory keep the mark so that the next report
                names them.";
            attribute :>> criterion = "Of an object with 3 pages committed: before
                any writable shared mapping, a report names 0 pages; with one made
                and gone, the next names 3 and the one after 0; with one alive,
                every report names 3; a report refused for memory is followed by one
                naming 3.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::vmo::Vmo::take_mapped_writes",
                "user::vmo::Vmo::raise_shared_may_write",
                "user::vmo::Vmo::writably_mapped");
        }
    }

    package AddressSpaces {
        doc /* src/kernel/src/user/space.rs: a process's address space -- the region
             * map and the page tables under it, the mappings of VMOs, files,
             * devices and windows, demand faults and copy-on-write, protect,
             * unmap, mremap and madvise, and the shootdown every takedown of a
             * translation runs before anything it reached is given back. */

        // --- Making a space, and what it refuses before anything is mapped ---

        requirement <'L.user.44'> anEmptySpaceHasARootAndNoRegions : ItemLowLevel {
            attribute :>> statement = "AddressSpace::new shall make a space holding no
                region, with its root table in a page-aligned frame.";
            attribute :>> criterion = "A fresh space holds 0 regions, and its root
                table's physical address is non-zero and a multiple of the page
                size.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "user::space::AddressSpace::new";
        }
        requirement <'L.user.45'> nothingIsMappedBelowTheFloor : ItemLowLevel {
            attribute :>> statement = "AddressSpace::new shall make a space whose map
                places no region below MMAP_MIN_ADDR, 64 KiB, whether asked for a fixed
                address or given a hint, and lets a region start at the floor
                itself.";
            attribute :>> criterion = "map_anonymous at address 0 is refused; an
                mmap hinted at 0x10 is placed at or above 64 KiB; a MAP_FIXED mmap at
                64 KiB lands there.";
            attribute :>> parent = "H.MEM.3";
            attribute :>> unit = ("user::space::AddressSpace::new",
                "user::space::AddressSpace::map_anonymous",
                "user::space::AddressSpace::map_anywhere");
        }
        requirement <'L.user.46'> aRegionOutsideTheUserHalfIsRefused : ItemLowLevel {
            attribute :>> statement = "map_anonymous shall refuse a range outside the
                user half with NotUserRange and insert nothing.";
            attribute :>> criterion = "A one-page mapping at the kernel half's base is
                refused NotUserRange, and the space still holds 0 regions.";
            attribute :>> parent = "H.MEM.2";
            attribute :>> unit = "user::space::AddressSpace::map_anonymous";
        }
        requirement <'L.user.47'> theKernelImageIsNoDeviceMemory : ItemLowLevel {
            attribute :>> statement = "map_device and map_window shall refuse a
                physical range that touches the kernel's image with Refused, whatever
                the caller holds, and insert nothing.";
            attribute :>> criterion = "A device mapping of the image's first page and
                a window of two pages straddling the image's start are each refused
                Refused, and the space holds 0 regions after both.";
            attribute :>> parent = "H.MEM.10";
            attribute :>> unit = ("user::space::check_device_range",
                "user::space::AddressSpace::map_device",
                "user::space::AddressSpace::map_window");
        }
        requirement <'L.user.48'> aDeviceRangeThatWrapsIsABadRange : ItemLowLevel {
            attribute :>> statement = "map_device and map_window shall refuse a
                physical range that wraps the address space with BadRange, before any
                other test, and insert nothing.";
            attribute :>> criterion = "The top page, and a whole-pages length from a
                low page that runs past the top, are each refused BadRange by
                map_device and by map_window, 4 of 4, and the space holds 0 regions.";
            attribute :>> parent = "H.MEM.10";
            attribute :>> unit = ("user::space::check_device_range",
                "user::space::AddressSpace::map_device",
                "user::space::AddressSpace::map_window");
        }
        requirement <'L.user.49'> aDeviceMappingIsWholePagesInTheUserHalf : ItemLowLevel {
            attribute :>> statement = "map_device shall refuse a length that is not
                whole pages with BadRange and a range reaching past the user half
                with NotUserRange.";
            attribute :>> criterion = "Device mappings of no length and of half a page
                are refused BadRange, and one of two pages from the user half's last
                page is refused NotUserRange at that address: 3 refusals of 3.";
            attribute :>> parent = "H.MEM.2";
            attribute :>> unit = ("user::space::check_device_range",
                "user::space::AddressSpace::map_device");
        }
        requirement <'L.user.50'> eachMappingCallRefusesWhatIsItsToRefuse : ItemLowLevel {
            attribute :>> statement = "map_anywhere shall refuse a length of zero,
                map_object an executable mapping and a range past the user half,
                map_file an offset that is not whole pages, and protect a range past
                the user half, each with its own error.";
            attribute :>> criterion = "map_anywhere of no length is BadRange;
                map_object executable is Refused and past the user half NotUserRange;
                map_file from half a page in is BadRange; protect of two pages from
                the user half's last page is NotUserRange: 5 refusals of 5.";
            attribute :>> parent = ("H.MEM.2", "H.MEM.18");
            attribute :>> unit = ("user::space::AddressSpace::map_anywhere",
                "user::space::AddressSpace::map_object",
                "user::space::AddressSpace::map_file",
                "user::space::AddressSpace::protect");
        }

        // --- Faults ---

        requirement <'L.user.51'> aFaultOutsideEveryRegionIsASegfault : ItemLowLevel {
            attribute :>> statement = "fault shall answer an address no region covers
                with NotMapped, which the process receives as SIGSEGV.";
            attribute :>> criterion = "A read fault at the first address past a
                two-page region is answered NotMapped.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::fault",
                "user::space::AddressSpace::fill_file_page",
                "user::space::AddressSpace::resolve",
                "user::space::SpaceError::fmt");
        }
        requirement <'L.user.52'> aWriteToAReadOnlyRegionIsRefused : ItemLowLevel {
            attribute :>> statement = "fault shall refuse a write to a region that does
                not permit writing with Refused, and resolve a read of it.";
            attribute :>> criterion = "In a read-only region a write fault is answered
                Refused and a read fault at the same address succeeds.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::fault",
                "user::space::AddressSpace::resolve", "user::space::permits");
        }
        requirement <'L.user.53'> anInaccessibleRegionIsNeverMapped : ItemLowLevel {
            attribute :>> statement = "fault shall refuse a read of a region that
                permits nothing with Refused and install no translation for it.";
            attribute :>> criterion = "A read fault in a PROT_NONE region is answered
                Refused, and the page does not translate in the space's tables
                afterwards.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::fault",
                "user::space::AddressSpace::resolve", "user::space::permits");
        }
        requirement <'L.user.54'> pagesArriveOnDemandAndGoBack : ItemLowLevel {
            attribute :>> statement = "An anonymous mapping shall commit no frame until
                a page is faulted, each fault shall map the faulting page and no
                other, an unmap shall give back the pages of its range and leave the
                rest mapped, and dropping the space shall give back every frame it
                held.";
            attribute :>> criterion = "Mapping 4 pages costs exactly 1 frame, the root;
                faulted out of order, each of the 4 translates and holds what is
                written to it; re-faulting a present page costs 0 frames; unmapping
                the first 2 gives back at least 2 frames, page 0 no longer translates
                and page 2 still does; with the space dropped the free count is what
                it was before, 0 frames kept.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.9");
            attribute :>> unit = ("user::space::AddressSpace::map_anonymous",
                "user::space::AddressSpace::add_region", "user::space::sharing_of",
                "user::space::AddressSpace::fault",
                "user::space::AddressSpace::resolve", "user::space::commit_page",
                "user::space::AddressSpace::unmap",
                "user::space::AddressSpace::take_down",
                "user::space::AddressSpace::give_back", "user::space::owner",
                "user::space::note", "user::space::AddressSpace::drop_unnamed",
                "user::space::drop_objects", "user::space::AddressSpace::drop");
        }

        // --- A space on a processor ---

        requirement <'L.user.55'> theProcessorWalksAnInstalledSpace : ItemLowLevel {
            attribute :>> statement = "install shall make the running processor
                translate user addresses through the space's own tables, on the
                first installation and on every one after an uninstall.";
            attribute :>> criterion = "Over 2 rounds of install, use and uninstall, a
                write through each of 2 user addresses reads back through the address
                and through the direct map's alias of the frame the space's tables
                name, for both pages in both rounds; and the space dropped keeps 0
                frames.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::install",
                "user::space::AddressSpace::uninstall",
                "user::space::this_logical_cpu");
        }
        requirement <'L.user.56'> twoSpacesReadTheirOwnPageAtOneAddress : ItemLowLevel {
            attribute :>> statement = "install and uninstall, as the scheduler calls
                them, shall leave a task reading its own space's frame at a user
                address however often one processor alternates between two spaces.";
            attribute :>> criterion = "Two tasks pinned to one processor, each in a
                space holding a different value at one address, yield between reads:
                128 reads of 128 return the reader's own value and 0 the other's (the
                `spaces` line), and the spaces dropped keep 0 frames.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::install",
                "user::space::AddressSpace::uninstall",
                "user::space::this_logical_cpu");
        }

        // --- fork and copy-on-write ---

        requirement <'L.user.57'> aSharedRegionIsOneObjectAcrossFork : ItemLowLevel {
            attribute :>> statement = "fork shall give the child the same object for a
                MAP_SHARED region, not a copy, so that the two spaces reach one frame
                with one holder and see each other's writes.";
            attribute :>> criterion = "After a fork of a space holding a written
                shared page, the child's write fault maps the parent's frame, the
                frame has 1 holder, a write through the child's frame reads back
                through the parent's, and the spaces dropped keep 0 frames.";
            attribute :>> parent = "H.MEM.16";
            attribute :>> unit = ("user::space::AddressSpace::fork",
                "user::space::shared_object", "user::space::attach_child",
                "user::space::sharing_of");
        }
        requirement <'L.user.58'> forkSharesAndAWriteCopies : ItemLowLevel {
            attribute :>> statement = "fork shall take the parent's translations of
                its private writable pages down and share their frames with the
                child, and a write to a page some other space
                still holds shall copy it into a frame of the writer's own, while a
                write to a page nobody else holds any more shall copy nothing.";
            attribute :>> criterion = "After a fork of 2 written private pages the
                parent translates neither; read faults leave both sides on the
                original frames with 2 holders each; the child's write, made with
                its space installed, lands in a new frame of 1 holder, leaves the
                original at 1 holder with the parent's value, and a second write
                copies nothing; with the parent dropped the child's write to the
                other page keeps its frame and contents; 0 frames kept.";
            attribute :>> parent = ("H.MEM.8", "H.MEM.9");
            attribute :>> unit = ("user::space::AddressSpace::fork",
                "user::space::AddressSpace::unmap_copied_on_write",
                "user::space::inherited", "user::space::attach_child",
                "user::space::AddressSpace::begin_shootdown",
                "user::space::AddressSpace::shoot",
                "user::space::AddressSpace::resolve", "user::space::copy_on_write",
                "user::space::AddressSpace::forget_in");
        }
        requirement <'L.user.59'> aKernelWriteToAnOwnPageTakesNothingDown : ItemLowLevel {
            attribute :>> statement = "with_page shall let a write go straight through
                to a copy-on-write page this space alone holds, taking no translation
                down.";
            attribute :>> criterion = "A kernel write through with_page to a page the
                child has already copied succeeds and begins 0 shootdowns of the
                child's space.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::with_page",
                "user::space::writable_in_place");
        }
        requirement <'L.user.60'> aPageMadeWritableAfterForkIsCopied : ItemLowLevel {
            attribute :>> statement = "A private page read-only at a fork and made
                writable afterwards with protect shall be copied by the side that
                writes it, the other keeping its own.";
            attribute :>> criterion = "A page written, protected read-only, forked and
                protected writable in the child is copied by the child's installed
                write: the child's frame differs from the parent's, holds the
                child's value, the parent's holds its own, and 0 frames are kept.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::protect",
                "user::space::AddressSpace::resolve", "user::space::copy_on_write");
        }
        requirement <'L.user.128'> protectKeepsAMappingShared : ItemLowLevel {
            attribute :>> statement = "protect shall change a region's read, write
                and execute permissions and keep whether it is shared.";
            attribute :>> criterion = "A memfd mapped shared and read-write and
                protected read-write again is written through the mapping: the
                byte is in the file (the memfd line).";
            attribute :>> parent = "H.MEM.16";
            attribute :>> unit = "user::space::AddressSpace::protect";
        }
        requirement <'L.user.129'> aWritableRegionReads : ItemLowLevel {
            attribute :>> statement = "fault shall permit a read of a region that
                permits writing, as Linux's protection_map makes a PROT_WRITE
                mapping readable, and still refuse a read of one that permits
                nothing.";
            attribute :>> criterion = "A memfd's page mapped shared with PROT_WRITE
                alone reads what the file holds (the memfd line), while a read
                of a PROT_NONE region is still refused (L.user.53).";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::fault",
                "user::space::permits");
        }
        requirement <'L.user.61'> aProgramsCopyOnWriteWriteIsItsOwn : ItemLowLevel {
            attribute :>> statement = "A write by a program's own instruction to a
                private page its fork still shares shall be copied for the side that
                wrote it and be seen by neither the other side nor the page.";
            attribute :>> criterion = "The `cow` program, whose parent and child each
                write a page both shared after the fork, exits 61 (the `cow` line).";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::fork",
                "user::space::AddressSpace::resolve", "user::space::copy_on_write",
                "user::space::AddressSpace::forget_in",
                "user::space::AddressSpace::begin_shootdown",
                "user::space::AddressSpace::shoot");
        }
        requirement <'L.user.62'> aProgramsSharedWriteReachesItsParent : ItemLowLevel {
            attribute :>> statement = "A forked program's write to a MAP_SHARED
                anonymous page shall reach its parent, a page the child faults in
                first included, and its write to a MAP_PRIVATE page shall not.";
            attribute :>> criterion = "The `shared` program exits 62 (the `shared`
                line).";
            attribute :>> parent = "H.MEM.16";
            attribute :>> unit = ("user::space::AddressSpace::fork",
                "user::space::attach_child", "user::space::shared_object",
                "user::space::AddressSpace::resolve", "user::space::commit_page");
        }

        // --- Reaching user memory from the kernel ---

        requirement <'L.user.63'> anUnmapWaitsForACopyHoldingItsPage : ItemLowLevel {
            attribute :>> statement = "with_page shall hold the page it translated for
                as long as its copy runs, so that an unmap of the page on another
                processor does not return before the copy ends.";
            attribute :>> criterion = "On a machine with two processors or more, a
                copy holding a page in with_page on one processor for 30 ms reads the
                page's byte, the unmap started on the other while it held had not
                returned when the copy ended, and it had returned once both tasks
                were gone (the `unmap` line).";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::with_page",
                "user::space::AddressSpace::unmap");
        }
        requirement <'L.user.64'> aLookupRefusesAWriteToAReadOnlyRegion : ItemLowLevel {
            attribute :>> statement = "with_present_page shall refuse a write in a
                region that does not permit writing with Refused.";
            attribute :>> criterion = "with_present_page asked to write in a read-only
                region is answered Refused at that address.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::with_present_page",
                "user::space::permits");
        }
        requirement <'L.user.65'> aLookupUnderALockNeverFaults : ItemLowLevel {
            attribute :>> statement = "with_present_page shall run nothing and answer
                that the page must be faulted first when the page is absent or, for a
                write, a copy-on-write page another space still holds or a private
                file mapping's page its shadow does not hold.";
            attribute :>> criterion = "with_present_page answers Ok(None), and its
                closure is not run, for an absent page, a write to a page a fork
                still shares, and a write to a file page a private mapping has only
                read; after a fault each answers Some.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::with_present_page",
                "user::space::writable_in_place");
        }

        // --- Device memory and windows ---

        requirement <'L.user.66'> aWindowIsRefusedAsDeviceMemoryIs : ItemLowLevel {
            attribute :>> statement = "map_window shall refuse an executable window
                with Refused, a length that is not whole pages with BadRange and a
                range reaching past the user half with NotUserRange.";
            attribute :>> criterion = "A window over a frame asked read-execute is
                Refused, of half a page BadRange, and of two pages from the user
                half's last page NotUserRange: 3 refusals of 3.";
            attribute :>> parent = ("H.MEM.2", "H.MEM.18");
            attribute :>> unit = ("user::space::AddressSpace::map_window",
                "user::space::AddressSpace::map_window_typed",
                "user::space::check_device_range");
        }
        requirement <'L.user.67'> aWindowKeepsItsKeeperWhileMapped : ItemLowLevel {
            attribute :>> statement = "A window's region shall keep what map_window was
                given to keep until no region names the window, and an unmap of
                another mapping shall let go of that mapping's file and not of the
                window's keeper.";
            attribute :>> criterion = "Mapped, the window holds its keeper (count 2)
                and is the highest mapping; a file mapping beside it unmapped leaves
                its open file at count 1 and the keeper at 2; the window unmapped
                leaves the keeper at 1.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("user::space::AddressSpace::map_window",
                "user::space::AddressSpace::unmap",
                "user::space::AddressSpace::give_back",
                "user::space::AddressSpace::drop_unnamed", "user::space::still_named",
                "user::space::naming", "user::space::Named::of",
                "user::space::Named::contains", "user::space::drop_objects");
        }
        requirement <'L.user.68'> aDeviceMappingReachesTheDevicesOwnPages : ItemLowLevel {
            attribute :>> statement = "A page of a device region shall be faulted in to
                the device's own physical page, in the space that mapped it and in a
                fork child of it alike.";
            attribute :>> criterion = "On a machine with a whole-page aperture,
                io_mapping_map's address, faulted, translates to the aperture's
                physical address in the process's space and in a fork child's.";
            attribute :>> parent = ("H.MEM.1", "H.DMA.5");
            attribute :>> unit = ("user::space::AddressSpace::map_device",
                "user::space::AddressSpace::fault_device",
                "user::space::AddressSpace::resolve",
                "user::space::AddressSpace::fork");
        }
        requirement <'L.user.108'> aPrefetchableApertureMapsWriteCombining : ItemLowLevel {
            attribute :>> statement = "io_mapping_map_combining shall map an
                aperture the device says is prefetchable write-combining, its pages
                faulted in to the device's own memory, and shall refuse an aperture
                of registers with INVALID_ARGS; on x86-64 it shall refuse every
                aperture with INVALID_ARGS unless every processor programmed its
                PAT with the write-combining entry.";
            attribute :>> criterion = "On x86-64, AArch64 and ARMv7-A -- each of
                whose test-boot machines has a virtio-pci function with a whole-page
                prefetchable aperture, the boot's handles line saying 1 mapped
                write-combining -- its write-combining mapping reaches the
                aperture's physical address and its page table entry reads
                write-combining (normal non-cacheable on the two Arm
                architectures), not device; a register aperture's is refused with
                INVALID_ARGS; on x86-64, with one processor's PAT counted as not
                programmed, the prefetchable aperture's is refused with
                INVALID_ARGS, and with that refusal dropped stage 9 fails naming
                it.";
            attribute :>> parent = ("H.MEM.1", "H.DMA.5");
            attribute :>> unit = ("syscall::native::io_mapping_map",
                "object::io_mapping::IoMapping::map_into",
                "object::io_mapping::IoMapping::prefetchable",
                "object::io_mapping::combining_ready",
                "object::io_mapping::count_one_unprogrammed_for_check",
                "user::space::AddressSpace::map_device",
                "user::space::AddressSpace::fault_device", "mm::flags_in");
        }
        requirement <'L.user.109'> oneMemoryTypePerDevicePage : ItemLowLevel {
            attribute :>> statement = "A page of device memory shall be mapped into
                user address spaces with one memory type at a time: map_device and
                map_window shall refuse a range any page of which a live user
                mapping holds as another of device, write-combining or cached with
                OtherMemoryType -- ALREADY_BOUND from io_mapping_map and
                io_mapping_map_combining, EINVAL from mmap of a render node's blob
                -- and a mapping's hold shall end only once no region of any
                address space maps it, after its unmap's shootdown has returned.
                Holds shall be counted in one claim per distinct range and type, an
                I/O mapping's range being its whole aperture and a window's its
                whole blob, so that no number of mappings adds a claim.";
            attribute :>> criterion = "On x86-64, AArch64 and ARMv7-A, with a
                prefetchable aperture mapped write-combining, io_mapping_map of it is
                ALREADY_BOUND and a cached window over it is OtherMemoryType; with
                it mapped as a cached window, and then uncached, after the
                write-combining mapping is unmapped, io_mapping_map_combining is
                ALREADY_BOUND; with every mapping of it unmapped it maps
                write-combining again; mapping the cached window again, whole and
                one page of it, leaves the number of claims unchanged; with the
                type comparison dropped from memory_type::hold, stage 9 fails
                naming the uncached mapping made beside the write-combining one,
                and with every hold given a claim of its own it fails naming the
                claim the window's second mapping made.";
            attribute :>> parent = ("H.MEM.1", "H.DMA.5");
            attribute :>> unit = ("user::memory_type::hold",
                "user::memory_type::MemoryType::of",
                "user::space::hold_type",
                "user::space::AddressSpace::map_device",
                "user::space::AddressSpace::map_window",
                "user::space::AddressSpace::give_back",
                "user::memory_type::Hold::drop", "user::memory_type::release",
                "user::memory_type::claims", "user::space::WindowPages::all",
                "user::space::WindowPages::check");
        }
        requirement <'L.user.126'> onlyPlainMemoryIsAnonymous : ItemLowLevel {
            attribute :>> statement = "Vmo::is_anonymous shall answer true for a
                VMO iff it has no filler and no file bound: plain anonymous
                memory, never a file's object on a disk or in memory, so that
                what asks it -- a chardev driver's dmabuf install -- refuses a
                file's VMO with INVALID_ARGS.";
            attribute :>> criterion = "In stage 10's chardev self-check, a
                dmabuf install of a memfd's VMO, its handle holding READ, WRITE,
                MAP and TRANSFER, is refused INVALID_ARGS, and one of a VMO from
                vmo_create is installed; with is_anonymous answering true for
                every VMO, stage 10 fails naming the memfd's dmabuf.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "user::vmo::Vmo::is_anonymous";
        }
        requirement <'L.user.132'> onlyTheDriversOwnVmoCombines : ItemLowLevel {
            doc /* K6 of docs/INTEL-GPU.md section 5; the consultant's K6 (1),
                 * ledger 2026-10-18 line 2352. A chardev driver answers a
                 * program's mmap with MAP_VMO | MAP_WRITE_COMBINING, naming a
                 * VMO of its own handle table (map_reply, load ring), and the
                 * item decides whether that VMO may be write-combining at all.
                 * x86-64 alone at first: each Arm architecture's attribute
                 * (normal non-cacheable) is still to be argued, so there the
                 * answer is refused as it was before K6. */
            attribute :>> statement = "Vmo::make_combining shall make a VMO
                write-combining for every user mapping from then on only on
                x86-64, where the processor has CLFLUSH and every processor
                programmed its PAT with the write-combining entry, and only for
                plain anonymous memory that is not coherent and that no address
                space maps; anywhere else, and for any other VMO, it shall
                refuse and leave the VMO as it was.";
            attribute :>> criterion = "Stage 10's chardev check: on x86-64 the
                driver's own anonymous VMO answered MAP_VMO with
                MAP_WRITE_COMBINING is made write-combining; on AArch64 and
                ARMv7-A the same answer is refused, the program's mmap EIO and
                the VMO unchanged; on every machine a memfd's VMO and a VMO an
                address space already maps are refused the same way. With the
                anonymous test or the mappers' test dropped, the boot panics.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.15");
            attribute :>> unit = ("user::vmo::Vmo::make_combining",
                "user::vmo::Vmo::is_combining", "arch::x86_64::framebuffer::user_combining");
        }
        requirement <'L.user.133'> aCombiningVmoIsOneTypeToPrograms : ItemLowLevel {
            doc /* The memory-type aliasing argument (the consultant's K6 (2),
                 * and ledger 2368 (1)-(3)). A write-combining VMO's frames
                 * stay write-back in the kernel's direct map, as i915 keeps an
                 * LLC part's objects while a program maps them
                 * write-combining. The SDM (Vol. 3A 11.12.4) makes that safe
                 * only if no write-back line of the page is left to be
                 * evicted over what write-combining stores put in memory, and
                 * no read through the write-back alias hits a stale line.
                 *
                 * (1) Every user mapping of the VMO, in every space, is
                 * write-combining, never also write-back beside it:
                 * AddressSpace::resolve, and typed() for a private mapping's
                 * read of the page.
                 * (2) Every path that reaches one of its frames through the
                 * direct map is flushed, refused or unreachable:
                 * - zeroing a new frame (Vmo::commit, Vmo::commit_run):
                 *   flushed after, under the pages' lock; the frames there
                 *   when make_combining sets the flag: flushed by it;
                 * - uaccess, so every syscall buffer, futex word and
                 *   put_user in such a page (AddressSpace::with_page,
                 *   with_present_page): coherent_touch flushes the page's
                 *   lines before the copy and after it, where this space maps
                 *   the page write-combining;
                 * - Vmo::read_present, read_page, write_page,
                 *   commit_and_write and zero_from: flushed before a read
                 *   and after a write;
                 * - vmo_read and vmo_write: refused BAD_STATE;
                 * - a private mapping's copy of the page (copy_or_zero):
                 *   flushed before the copy;
                 * - the quarantine's fold of a dead driver's pinned frame
                 *   (object::pin::fold): flushed before;
                 * - a block or net ring, an audio buffer, a display card's
                 *   buffer, which a kernel core reads and writes for its life:
                 *   marked by Vmo::take_kernel_view, which make_combining
                 *   then refuses, and which refuses an object already
                 *   write-combining, so the two never meet;
                 * - copy-on-write copies of a shared frame (copy_on_write,
                 *   Pages::exclusive, fork_private's copy of a held page):
                 *   unreachable, since a write-combining VMO is mapped only
                 *   shared and never forked into a private object;
                 * - a core dump, /proc/<pid>/mem and process_vm_readv: not
                 *   built.
                 * Drain before reuse, by code: a frame of the VMO goes back
                 * only after AddressSpace::begin_shootdown's set of
                 * processors has answered smp::flush_tlb_pages (each one
                 * interrupted and running INVLPG, waited for in
                 * invalidate_pages); a processor leaves a space's set only
                 * after AddressSpace::install or uninstall has loaded another
                 * root into CR3, with or without a PCID's flush (docs/PCID.md).
                 * The interrupt, INVLPG and the CR3 load each drain the
                 * write-combining buffers (SDM Vol. 3A 11.3.1), so no buffered
                 * store reaches a frame after it is reused. */
            attribute :>> statement = "Every user mapping of a page of a
                write-combining VMO shall be write-combining; each of its frames
                shall have its cache lines written back and invalidated after
                the kernel zeroed it and before the VMO can be mapped
                write-combining over it; every copy the kernel makes to or from
                such a page through the direct map shall flush the page's lines
                before it and after it; vmo_read and vmo_write of the VMO shall
                be refused BAD_STATE; and no kernel core that reads a VMO
                through the direct map for its life shall take one that is
                write-combining, nor shall one it took become so.";
            attribute :>> criterion = "Stage 10's chardev check on x86-64: a
                program's mapping of the VMO, faulted in by a write, reads back
                write-combining and not device; a write(2) from it and a
                read(2) into it through a memfd round-trip its bytes and count
                four coherent touches; vmo_read of the VMO is BAD_STATE. With
                resolve's write-combining choice, the coherent touch or
                vmo_copy's refusal dropped, the boot panics. The flush's effect
                is argued, since QEMU models no cache.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.15");
            attribute :>> unit = ("user::vmo::Vmo::commit", "user::vmo::Vmo::commit_run",
                "user::vmo::flush_frame", "user::vmo::Vmo::flush_if_combining",
                "user::vmo::Vmo::take_kernel_view", "user::vmo::Vmo::read_present",
                "user::vmo::Vmo::read_page", "user::vmo::Vmo::write_page",
                "user::vmo::Vmo::commit_and_write", "user::vmo::Vmo::zero_from",
                "user::space::AddressSpace::resolve",
                "user::space::AddressSpace::fault_private_file", "user::space::typed",
                "user::space::AddressSpace::with_page",
                "user::space::AddressSpace::with_present_page",
                "user::space::AddressSpace::coherent_touch", "user::space::copy_or_zero",
                "object::pin::fold", "syscall::native::vmo_copy",
                "arch::x86_64::framebuffer::flush_cache_lines",
                "arch::x86_64::framebuffer::flush_lines");
        }
        requirement <'L.user.107'> aCopyThroughADevicePageIsRefused : ItemLowLevel {
            attribute :>> statement = "with_page and with_present_page shall refuse
                a copy to or from a page of a device region with Refused, before the
                page is faulted in or a direct-map address is formed for it, and
                shall form one only through direct_map_ram.";
            attribute :>> criterion = "A writev() from and a readv() into a device's
                aperture mapped by io_mapping_map, a cached window past all RAM and,
                where the memory map has a hole between runs of RAM, a window over
                it are each EFAULT with the page still absent, and with_present_page
                refuses each for reading and writing; with the aperture's page
                faulted in, both copies are EFAULT again (the `copies` line).";
            attribute :>> parent = ("H.MEM.1", "H.DMA.5");
            attribute :>> unit = ("user::space::AddressSpace::with_page",
                "user::space::AddressSpace::with_present_page", "user::space::copyable");
        }

        // --- mremap ---

        requirement <'L.user.69'> mremapRefusesWhatItShould : ItemLowLevel {
            attribute :>> statement = "remap shall refuse a new length of zero with
                BadRange, an old range longer than its region with NotMapped and a
                fixed destination overlapping the old range with BadRange, and leave
                a remap to the same length where it is.";
            attribute :>> criterion = "On a two-page region, a remap to length 0 is
                BadRange, of three pages NotMapped, to a fixed address one page in
                BadRange -- 3 refusals of 3 -- and to the same length anywhere
                answers the region's own address.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::remap",
                "user::space::remap_target", "user::space::user_range");
        }
        requirement <'L.user.70'> aRegionGrowsWhereItHasRoom : ItemLowLevel {
            attribute :>> statement = "remap shall grow a region where it is when the
                pages after it are free, even when it may move.";
            attribute :>> criterion = "A one-page region with free pages after it,
                remapped to three pages anywhere, answers its own address.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::remap",
                "user::space::remap_target", "user::space::grows_in_place",
                "user::space::AddressSpace::move_private",
                "user::space::AddressSpace::add_fresh",
                "user::space::AddressSpace::remap_backing");
        }
        requirement <'L.user.71'> aSharedRegionNeverGrowsOverItsOwnPages : ItemLowLevel {
            attribute :>> statement = "remap shall refuse with OutOfMemory to grow a
                shared region over offsets of its object that another region of the
                space already names.";
            attribute :>> criterion = "A three-page shared region whose last page was
                moved away on its own cannot grow its first two pages back to three
                in place: OutOfMemory.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.9");
            attribute :>> unit = ("user::space::AddressSpace::remap",
                "user::space::named_elsewhere");
        }
        requirement <'L.user.72'> aHeldPageDoesNotMove : ItemLowLevel {
            attribute :>> statement = "remap shall refuse with OutOfMemory to move a
                private region that holds a page a device holds.";
            attribute :>> criterion = "A written one-page region whose page is held,
                with a neighbour in the way, is refused OutOfMemory when remapped to
                two pages anywhere.";
            attribute :>> parent = "H.DMA.3";
            attribute :>> unit = ("user::space::AddressSpace::remap",
                "user::space::AddressSpace::move_private",
                "user::space::AddressSpace::add_fresh",
                "user::space::AddressSpace::drop_fresh");
        }
        requirement <'L.user.73'> mremapCarriesTheContents : ItemLowLevel {
            attribute :>> statement = "remap shall refuse to grow a region in the way
                of a neighbour unless it may move, and shall carry a region's
                contents with it when it moves it, grow it with zeros and shrink it
                in place, leaving nothing readable where it no longer is.";
            attribute :>> criterion = "Two written pages with a neighbour after them:
                growing to four without MREMAP_MAYMOVE is ENOMEM; with it they move,
                a string in the first page and one across the page boundary read
                back at the new address, the old address is unreadable and the
                fourth page reads 0; shrunk to one page in place it keeps the first
                page's string and its tail is unreadable; moved to the old address
                with MREMAP_FIXED it lands there with the string.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.6");
            attribute :>> unit = ("user::space::AddressSpace::remap",
                "user::space::remap_object", "user::space::remap_target",
                "user::space::grows_in_place", "user::space::user_range",
                "user::space::AddressSpace::move_private",
                "user::space::AddressSpace::add_fresh",
                "user::space::AddressSpace::remap_backing",
                "user::space::AddressSpace::clear_for_remap",
                "user::space::AddressSpace::take_down",
                "user::space::AddressSpace::give_back");
        }

        // --- Native objects and shared code ---

        requirement <'L.user.74'> aNativeRegionIsNotReshaped : ItemLowLevel {
            attribute :>> statement = "protect and remap shall refuse with Refused a
                region vmo_map made, whose protection and size are its handle's.";
            attribute :>> criterion = "A read-only vmo_map region is refused Refused
                by protect to read-write and by remap to two pages: 2 refusals of
                2.";
            attribute :>> parent = "H.OBJ.2";
            attribute :>> unit = ("user::space::AddressSpace::protect",
                "user::space::AddressSpace::remap", "user::space::Inner::reshapeable");
        }
        requirement <'L.user.75'> aMappedVmoIsTheVmo : ItemLowLevel {
            attribute :>> statement = "map_object shall map the object itself, shared,
                so that its bytes and the mapping's are the same bytes, a fork child
                writes into the same object, and the mapping outlives the handle it
                was made with.";
            attribute :>> criterion = "A VMO mapped at the address asked shows bytes
                vmo_write put there; bytes written through the mapping, and through a
                fork child's, are read back by vmo_read or the parent; with every
                handle to the VMO closed the mapping still shows its bytes.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::map_object",
                "user::space::AddressSpace::add_region",
                "user::space::AddressSpace::fork", "user::space::shared_object",
                "user::space::attach_child");
        }
        requirement <'L.user.76'> theVdsoIsReadAndRunOverReadOnlyData : ItemLowLevel {
            attribute :>> statement = "map_shared_code shall map a two-page object as a
                read-only shared data page below a read-and-run shared code page,
                neither writable nor able to be made writable.";
            attribute :>> criterion = "Where the architecture has a vDSO, a space it
                is mapped into shows the code page read, execute, shared and not
                writable over a data page read, shared and neither, the image reads
                as ELF, and a write through with_page and a protect to read-write
                are each refused on both pages, 4 of 4.";
            attribute :>> parent = "H.MEM.18";
            attribute :>> unit = ("user::space::AddressSpace::map_shared_code",
                "user::space::AddressSpace::map_shared_code_near",
                "user::space::AddressSpace::add_region",
                "user::space::AddressSpace::with_page", "user::space::permits",
                "user::space::AddressSpace::protect");
        }
        // Split from L.user.76 (the consultant's T1, ledger 2026-10-14 line
        // 966): the vDSO view swapped in place for a space entering a time
        // namespace (docs/NAMESPACES.md section 12.1).
        requirement <'L.user.130'> aSharedCodeViewIsReplacedInPlace : ItemLowLevel {
            attribute :>> statement = "replace_shared_code shall give a space
                another two-page shared code object at the place the space maps
                the first, the first unmapped and the new one mapped as
                map_shared_code maps one, leaving every other space's view
                alone.";
            attribute :>> criterion = "A space forked into a time namespace
                that shifts a clock has the shifted object mapped at the first
                one's place and the first unmapped, the parent's mapping
                unchanged, and a grandchild in its parent's namespace keeps its
                parent's view (the `timens` line).";
            attribute :>> parent = "H.MEM.18";
            attribute :>> unit = ("user::space::AddressSpace::replace_shared_code",
                "user::space::AddressSpace::map_shared_code_near");
        }
        requirement <'L.user.131'> theVdsoIsClearOfWinesRange : ItemLowLevel {
            attribute :>> statement = "map_shared_code shall map a two-page object to
                end at the address it is asked for when that range is free; the
                vDSO is asked for 128 MiB below the stack's top, below the stack's
                guard and outside 0x7ffffe000000..0x7fffffff0000, the range Wine's
                x86-64 preloader reserves, so that a program it starts keeps
                AT_SYSINFO_EHDR.";
            attribute :>> criterion = "On x86-64 and AArch64, the architectures with a
                vDSO, a space a program was loaded into as exec loads it maps the vDSO's
                image at exec::VDSO_END less a page, outside Wine's range, with the
                stack's guard the next region above it (the `vdso` line).";
            attribute :>> parent = "H.SCHED.9";
            attribute :>> unit = ("user::space::AddressSpace::map_shared_code",
                "user::space::AddressSpace::map_shared_code_near");
        }
        requirement <'L.user.77'> aSpaceFindsItsTrampoline : ItemLowLevel {
            attribute :>> statement = "shared_code_at shall answer where
                map_shared_code put an object in the space, so that a signal's return
                trampoline is found at its place in the mapped vDSO.";
            attribute :>> criterion = "On an architecture whose vDSO holds only its
                signal return trampoline, a fresh space the vDSO is mapped into finds
                the trampoline at the mapped image plus the trampoline's offset (the
                `vdso` line).";
            attribute :>> parent = "H.TRAP.4";
            attribute :>> unit = "user::space::AddressSpace::shared_code_at";
        }
        requirement <'L.user.78'> aSharedWordIsOneFutexAcrossFork : ItemLowLevel {
            attribute :>> statement = "shared_object_at shall name, for an address in a
                shared region, the object and byte offset every space mapping that
                object names it by.";
            attribute :>> criterion = "A parent waiting without FUTEX_PRIVATE_FLAG on a
                MAP_SHARED word is not woken by its fork child's private-keyed wake
                of the same word and is woken by the child's shared wake.";
            attribute :>> parent = "H.MEM.16";
            attribute :>> unit = "user::space::AddressSpace::shared_object_at";
        }

        // --- madvise ---

        requirement <'L.user.79'> droppedAnonymousPagesGoBackAtOnce : ItemLowLevel {
            attribute :>> statement = "advise with Discard or Free on private anonymous
                memory shall give the range's frames back at once and leave the
                range mapped, reading zeros, its neighbours untouched; a hint shall
                drop nothing.";
            attribute :>> criterion = "Of 10 written pages, MADV_DONTNEED of pages 1 to
                8 gives back exactly 8 frames, lowers the resident count by 8, pages
                1 to 8 read 0 and pages 0 and 9 their pattern; MADV_FREE the same;
                12 hints over all 10 pages each answer 0 and every page keeps its
                pattern.";
            attribute :>> parent = ("H.MEM.9", "H.MEM.6");
            attribute :>> unit = ("user::space::AddressSpace::advise",
                "user::space::AddressSpace::advise_region",
                "user::space::Advised::take",
                "user::space::AddressSpace::begin_shootdown",
                "user::space::AddressSpace::shoot",
                "user::space::AddressSpace::resident_pages");
        }
        requirement <'L.user.80'> aDroppedPageKeepsItsForkCopy : ItemLowLevel {
            attribute :>> statement = "advise dropping a page a fork left shared shall
                take only this space's reference, so the other space keeps the
                page's contents.";
            attribute :>> criterion = "After a fork, the parent's MADV_DONTNEED of a
                written page leaves the parent reading 0 there and the child reading
                the page's pattern.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::advise",
                "user::space::AddressSpace::advise_region",
                "user::space::Advised::take");
        }
        requirement <'L.user.81'> adviceOnAFileMappingSparesTheFile : ItemLowLevel {
            attribute :>> statement = "advise shall drop a private file mapping's
                copies and show the file there again, drop only a shared file
                mapping's translations, take no page out of the file, and refuse
                Free and Remove on a file mapping as Linux does.";
            attribute :>> criterion = "Two copied pages of a private memfd mapping,
                dropped, give back exactly 2 frames and show the file's bytes; a
                shared mapping dropped still shows what it wrote, as the file does;
                MADV_FREE is EINVAL on both, MADV_REMOVE EACCES on the private and
                EOPNOTSUPP on the shared; the file's committed count is unchanged.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("user::space::AddressSpace::advise",
                "user::space::AddressSpace::advise_region",
                "user::space::Advised::take", "user::space::removable");
        }
        requirement <'L.user.82'> aHoleIsPunchedInSharedMemory : ItemLowLevel {
            attribute :>> statement = "advise with Remove on shared anonymous memory
                shall take the range's pages out of the object, so every space that
                maps it reads zeros there and the frames go back, and Discard there
                shall keep the contents.";
            attribute :>> criterion = "Of 4 written shared pages seen by a fork child,
                MADV_REMOVE of pages 1 and 2 gives back exactly 2 frames, both spaces
                read 0 there and the pattern at pages 0 and 3; MADV_DONTNEED of page
                0 keeps its pattern; MADV_FREE there is EINVAL.";
            attribute :>> parent = ("H.MEM.6", "H.MEM.9");
            attribute :>> unit = ("user::space::AddressSpace::advise",
                "user::space::AddressSpace::advise_region", "user::space::removable");
        }
        requirement <'L.user.83'> adviceOverAHoleAdvisesTheMappedPart : ItemLowLevel {
            attribute :>> statement = "advise shall advise every mapped part of a range
                and then answer Unmapped for a part no region maps, and shall refuse
                Remove on private anonymous memory as Invalid.";
            attribute :>> criterion = "MADV_REMOVE on private anonymous memory is
                EINVAL; madvise of 0 bytes answers 0; over two pages whose second is
                unmapped MADV_DONTNEED is ENOMEM and the first page reads 0 after,
                and MADV_WILLNEED is ENOMEM.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("user::space::AddressSpace::advise",
                "user::space::AddressSpace::advise_region", "user::space::removable");
        }
        requirement <'L.user.84'> lockedAndDeviceRegionsRefuseAdvice : ItemLowLevel {
            attribute :>> statement = "advise shall refuse as Invalid any advice that
                drops pages on a device region, and Discard without locked_too, Free
                and Remove on an mlocked region, dropping nothing there.";
            attribute :>> criterion = "MADV_DONTNEED, MADV_FREE and MADV_REMOVE on an
                mlocked region and MADV_DONTNEED on an I/O mapping are each EINVAL,
                4 of 4, with 0 frames given back and the pages' contents kept;
                MADV_DONTNEED_LOCKED on the mlocked region drops it.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = "user::space::AddressSpace::advise_region";
        }
        requirement <'L.user.85'> adviceRefusedForMemoryChangesNothing : ItemLowLevel {
            attribute :>> statement = "advise that has no memory to note what it would
                take shall answer NoMemory before taking the region's pages or
                translations, the parts before it already advised.";
            attribute :>> criterion = "With each allocation of an MADV_DONTNEED over
                three regions failed in turn, each run answers 0 or EAGAIN, the pages
                of the refused region and after it keep their contents, and 0 frames
                are kept.";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("user::space::Advised::take",
                "user::space::AddressSpace::advise_region");
        }

        // --- protect ---

        requirement <'L.user.86'> protectTakesTheOldTranslationDown : ItemLowLevel {
            attribute :>> statement = "protect shall take the range's translations out
                of the tables, so the next access is judged by the new permissions,
                and keep the pages' contents.";
            attribute :>> criterion = "After mprotect of a written page to read-only
                the page does not translate, a copy into it is refused, and a copy
                out of it reads what was written.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::protect",
                "user::space::AddressSpace::begin_shootdown",
                "user::space::AddressSpace::shoot");
        }
        requirement <'L.user.87'> protectReachesTheCallersProcessor : ItemLowLevel {
            attribute :>> statement = "protect shall invalidate the old translation on
                the processor that made the call, so the program's next write to a
                page it made read-only faults.";
            attribute :>> criterion = "The `mprotect` program, pinned to another
                processor where there is one, writes a page, makes it read-only and
                writes again, and is ended by SIGSEGV (status 139), not exit 1.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::protect",
                "user::space::AddressSpace::shoot");
        }
        requirement <'L.user.88'> protectReachesAnotherProcessor : ItemLowLevel {
            attribute :>> statement = "protect shall reach every processor that may
                hold the space's translations, so that a thread on another processor
                cannot write through the writable entry it cached before.";
            attribute :>> criterion = "On a machine with two processors or more, a
                child that wrote a shared page and holds still on its processor has
                the page protected read-only from this one: the page no longer
                translates, and the child's next write ends it with SIGSEGV, 11,
                instead of answering (the `rmap` line).";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::protect",
                "user::space::AddressSpace::begin_shootdown",
                "user::space::AddressSpace::shoot");
        }
        requirement <'L.user.89'> anUnwritableFileMappingStaysUnwritable : ItemLowLevel {
            attribute :>> statement = "protect shall refuse with Refused to make
                writable a range reaching a shared file mapping that may not write its
                file, before changing anything.";
            attribute :>> criterion = "mprotect to read-write of a shared read-only
                mapping of a file opened read-only answers EACCES.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "user::space::AddressSpace::protect";
        }

        // --- Pages a VMO takes away, and a truncation ---

        requirement <'L.user.90'> aPageTakenAwayIsForgottenInEverySpace : ItemLowLevel {
            attribute :>> statement = "forget_runs shall take down every translation
                the space has of the object's named pages and answer the processors
                that may still cache one, so that the object's shootdown reaches them
                before the frame goes back.";
            attribute :>> criterion = "On a machine with two processors or more, with
                a shared page touched by a parent and its fork child on two
                processors, decommitting it leaves it translated in neither space;
                the given-up frame, claimed and poisoned, is not what the child then
                reads (it reads 0) or writes (the poison stays whole), and the page
                comes back in another frame.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::forget_runs",
                "user::space::AddressSpace::forget_in",
                "user::space::AddressSpace::begin_shootdown");
        }
        requirement <'L.user.91'> aPendingShootdownWidensTheForget : ItemLowLevel {
            attribute :>> statement = "forget_runs shall ask for the whole TLB of the
                space's processors to be flushed, and answer them, whenever a
                shootdown of the space has begun and not returned, even when it found
                no translation to take down.";
            attribute :>> criterion = "A decommit of a page whose translation a
                concurrent unmap has taken out of the tables, but whose shootdown has
                not returned, is answered with the space's processors and a
                whole-TLB flush, and the frame is claimed back only after that
                flush.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::forget_runs",
                "user::space::AddressSpace::begin_shootdown");
        }
        requirement <'L.user.92'> aTruncationReachesBothKindsOfFileMapping : ItemLowLevel {
            attribute :>> statement = "A page of a file mapping wholly past the file's
                end shall be refused, shared or private, and a
                truncation shall take a private mapping's copies past the cut, so the
                file grown back shows zeros there.";
            attribute :>> criterion = "A file of 2 pages and 100 bytes, mapped 4 pages
                shared and 4 private and cut to 1 page: page 0 still reads, pages 1
                to 3 are refused in each mapping, 3 of 3 and 3 of 3; grown back to 2
                pages, page 1 reads 0 in both.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.6");
            attribute :>> unit = ("user::space::commit_page",
                "user::space::AddressSpace::fault_private_file",
                "user::space::AddressSpace::forget_runs", "user::space::cut_shadow");
        }

        // --- File mappings ---

        requirement <'L.user.93'> aSharedFileMappingIsTheFile : ItemLowLevel {
            attribute :>> statement = "A shared file mapping shall show the file's own
                pages, so that a write through it is what the file reads back and a
                write to the file is what it shows.";
            attribute :>> criterion = "A shared mapping of a file of 2 pages and 100
                bytes shows those bytes and zeros to the end of the third page; bytes
                written through it are what the file reads back, and bytes written
                to the file are what it shows.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::map_file",
                "user::space::AddressSpace::add_file", "user::space::file_placement",
                "user::space::AddressSpace::resolve", "user::space::commit_page");
        }
        requirement <'L.user.94'> aPrivateFileMappingKeepsItsWrites : ItemLowLevel {
            attribute :>> statement = "A private file mapping shall show the file until
                it writes a page and then keep that page to itself, so that neither
                its writes nor a read into it reach the file or a shared mapping,
                while the file's writes still show through pages it has not
                copied.";
            attribute :>> criterion = "The private mapping shows the file's first page;
                a write through it and a read() into it each show in it and in
                neither the file nor the shared mapping; a write to the file shows
                through a page it never copied and not through one it did.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::map_file",
                "user::space::AddressSpace::add_file",
                "user::space::AddressSpace::fault_private_file",
                "user::space::AddressSpace::install_page",
                "user::space::AddressSpace::with_page",
                "user::space::writable_in_place");
        }
        requirement <'L.user.95'> aForkKeepsPrivateFileCopiesApart : ItemLowLevel {
            attribute :>> statement = "fork shall give the child the private file
                mapping's copies, and the child's write into one shall reach neither
                the parent nor the file.";
            attribute :>> criterion = "A fork child reads the parent's private copy;
                its write into a page the parent copied before the fork is seen by
                neither the parent nor the file.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::fork",
                "user::space::attach_child",
                "user::space::AddressSpace::fault_private_file",
                "user::space::AddressSpace::copy_shadow_page",
                "user::space::writable_in_place");
        }
        requirement <'L.user.96'> aProgramsWriteToAReadFilePageCopiesIt : ItemLowLevel {
            attribute :>> statement = "A program's write to a private file mapping's
                page it has only read shall copy the page and go through, not fault
                on the file's read-only page forever, and shall not reach the file.";
            attribute :>> criterion = "A program reads a private file page then writes
                it, and answers within 10 s with the file's word before and its
                marker after; forked and the child dropped, its next write answers
                too; the file's first word is unchanged.";
            attribute :>> parent = "H.MEM.8";
            attribute :>> unit = ("user::space::AddressSpace::fault_private_file",
                "user::space::AddressSpace::install_page");
        }
        requirement <'L.user.97'> aLoaderMapsItsFileAsItNeeds : ItemLowLevel {
            attribute :>> statement = "map_file shall place a fixed private mapping
                exactly where asked, showing the file, an executable one not
                writable, and a writable one made read-only by protect keeping what
                was written.";
            attribute :>> criterion = "A fixed read-execute file mapping lands at its
                address, shows the file's first page and refuses a write; a fixed
                read-write one lands next to it, takes a write, and after mprotect to
                read-only keeps it and refuses the next write.";
            attribute :>> parent = ("H.MEM.1", "H.MEM.18");
            attribute :>> unit = ("user::space::AddressSpace::map_file",
                "user::space::file_placement",
                "user::space::AddressSpace::fault_private_file",
                "user::space::AddressSpace::protect");
        }
        requirement <'L.user.98'> procMapsNamesAFileMapping : ItemLowLevel {
            attribute :>> statement = "with_regions shall show each region with its
                permissions, share mode and file, so that /proc/<pid>/maps names a
                shared writable file mapping by its file.";
            attribute :>> criterion = "/proc/<pid>/maps of the check's process has a
                line marked rw-s that ends in the mapped file's path.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "user::space::AddressSpace::with_regions";
        }

        // --- Memory charged, failures and giving everything back ---

        requirement <'L.user.99'> faultsStopAtTheJobsMemoryLimit : ItemLowLevel {
            attribute :>> statement = "fault shall be refused when the frame or page
                table it needs would pass the running task's job's memory limit,
                without holding back a sibling job, and a space's drop shall give
                every frame and table it was charged for back.";
            attribute :>> criterion = "Faulting a space in page by page as a task of a
                job limited to 48 pages' worth stops short of 48 pages, the refusal
                counted and the job's memory within one page of its limit; a sibling
                faults all 48 in; with both spaces dropped every level's memory and
                kernel memory read 0 (the `quota` line).";
            attribute :>> parent = "H.QUOTA.2";
            attribute :>> unit = ("user::space::AddressSpace::fault",
                "user::space::AddressSpace::resolve", "user::space::commit_page",
                "user::space::AddressSpace::drop");
        }
        requirement <'L.user.100'> aFailedAllocationLeavesNothingBehind : ItemLowLevel {
            attribute :>> statement = "Every address space operation that allocates --
                fork, the maps, fault, remap and unmap -- shall answer a refused
                allocation with OutOfMemory or absorb it, and keep no frame.";
            attribute :>> criterion = "With the n-th allocation failed alone for every
                n until a run in which none was, each run of the fork, move, shared,
                split, cut, object, stacks, code and page scenarios ends in success or out
                of memory, never another refusal, the run with nothing failed
                succeeds, and each scenario's frame window keeps 0 frames (the
                `sweep` line).";
            attribute :>> parent = "H.MEM.11";
            attribute :>> unit = ("user::space::AddressSpace::add_region",
                "user::space::AddressSpace::abandon_region", "user::space::unrooted",
                "user::space::inherited", "user::space::attach_child",
                "user::space::map_error", "user::space::remap_object",
                "user::space::AddressSpace::fork",
                "user::space::AddressSpace::map_file",
                "user::space::AddressSpace::add_file",
                "user::space::AddressSpace::map_window",
                "user::space::AddressSpace::map_object",
                "user::space::AddressSpace::map_shared_code",
                "user::space::AddressSpace::add_fresh",
                "user::space::AddressSpace::drop_fresh",
                "user::space::AddressSpace::move_private",
                "user::space::AddressSpace::remap_backing", "user::space::note",
                "user::space::AddressSpace::give_back",
                "user::space::AddressSpace::drop_unnamed", "user::space::Named::of",
                "user::space::drop_objects", "user::space::AddressSpace::drop");
        }
        requirement <'L.user.101'> stageSixKeepsNoFrameAndNoTable : ItemLowLevel {
            attribute :>> statement = "The address space operations stage 6 checks
                shall give back every frame they took and every page table their
                takedowns emptied, none left on a shootdown list dropped
                unreleased.";
            attribute :>> criterion = "After stage 6's object and address space checks
                0 page tables are counted kept (mm::tables_kept), and the held frame
                count is what it was before them, 0 frames leaked (the `objects`
                line).";
            attribute :>> parent = ("H.MEM.9", "H.MEM.7");
            attribute :>> unit = ("user::space::AddressSpace::unmap",
                "user::space::AddressSpace::take_down",
                "user::space::AddressSpace::protect",
                "user::space::AddressSpace::unmap_copied_on_write",
                "user::space::AddressSpace::clear_for_remap",
                "user::space::AddressSpace::shoot",
                "user::space::AddressSpace::drop");
        }

        // --- Needing a check ---

        requirement <'L.user.102'> theCeilingBoundsEveryMapping : ItemLowLevel {
            attribute :>> statement = "set_ceiling shall move the top of the window
                every mapping may reach, refusing an end past the user half or below
                a region still mapped and changing nothing then.";
            attribute :>> criterion = "With the ceiling set to 4 GiB less a page, an
                anywhere mapping lands below it and a fixed one reaching past it is
                refused; setting it past the user half, or below a mapped region, is
                refused BadRange and ceiling() reads as before.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = "user::space::AddressSpace::set_ceiling";
        }
        requirement <'L.user.103'> aDiskFilesPageIsReadBeforeItIsMapped : ItemLowLevel {
            attribute :>> statement = "fault shall have a file's object read a page of
                a file on a disk before mapping it, and answer a page the filesystem
                could not read with Unreadable and one it had no memory for with
                OutOfMemory.";
            attribute :>> criterion = "A shared and a private mapping of a file on a
                disk show its bytes, not zeros, on their first touch; a page whose
                read fails ends the program with SIGBUS.";
            attribute :>> parent = "H.MEM.1";
            attribute :>> unit = ("user::space::AddressSpace::fill_file_page",
                "user::space::AddressSpace::fill_file_pages",
                "user::space::AddressSpace::fill_file_page_alone");
        }
        requirement <'L.user.104'> aFaultIsResolvedWithNoPreemptionLockHeld : ItemLowLevel {
            attribute :>> statement = "fault shall stop a debug kernel, naming the
                outermost lock's site, when it is asked to resolve a fault while a
                lock that disables preemption is held.";
            attribute :>> criterion = "In a debug build, a fault asked for while a
                spin lock is held stops the kernel with the assertion's message and
                that lock's file and line.";
            attribute :>> parent = "H.FAIL.1";
            attribute :>> unit = ("user::space::fault_requested",
                "user::space::AddressSpace::fault");
        }
        requirement <'L.user.105'> anUnmapFreesNothingBeforeItsShootdown : ItemLowLevel {
            attribute :>> statement = "unmap shall give back no page of its range, and
                no page table it emptied, before the shootdown covering them has
                returned from every processor in the space's set.";
            attribute :>> criterion = "On a machine with two processors or more, a
                page a program on another processor keeps reading is unmapped from
                this one; its frame, claimed back and poisoned the moment it is
                free, is never what the program reads: 0 poisoned reads, and the
                program faults.";
            attribute :>> parent = ("H.MEM.7", "H.MEM.17");
            attribute :>> unit = ("user::space::AddressSpace::unmap",
                "user::space::AddressSpace::take_down",
                "user::space::AddressSpace::give_back",
                "user::space::AddressSpace::begin_shootdown",
                "user::space::AddressSpace::shoot");
        }
        requirement <'L.user.106'> aCodePageIsReadAndRunAlone : ItemLowLevel {
            attribute :>> statement = "map_code_page shall map a one-page object as a
                single read-and-run shared page, never writable, and refuse an object
                of any other size before changing the space.";
            attribute :>> criterion = "A two-page object is refused with BadRange and
                the space keeps its regions; a one-page object maps as one region
                read, execute and shared and not writable, one page long, and
                unmapping it leaves the space's regions as they were (the sweep's
                page scenario, the `sweep` line).";
            attribute :>> parent = "H.MEM.18";
            attribute :>> unit = "user::space::AddressSpace::map_code_page";
        }
        requirement <'L.user.169'> aSpaceIsLeftOnlyInAnOrderAShootdownSees : ItemLowLevel {
            doc /* docs/PCID.md section 3; the consultant's P2, ledger line 4734. */
            attribute :>> statement = "begin_shootdown shall count a shootdown before
                it reads the space's set, and install shall join the set before it
                reads the count, both sequentially consistent, so that a processor
                that left the set with the space's translations kept under a PCID
                is either interrupted by a shootdown or flushes the PCID before it
                uses the space again.";
            attribute :>> criterion = "The loom model of the two orders
                (src/tests/loom/tests/pcid.rs, `cargo xtask loom`) finds no
                interleaving in which a processor uses a translation after the
                shootdown that took it down has returned; with either order
                reversed, or the slot's comparison left out, it finds one. The
                switch-back check runs both orders on every boot.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::install",
                "user::space::AddressSpace::begin_shootdown");
        }
        requirement <'L.user.170'> aSpaceSwitchedBackToSeesItsShootdowns : ItemLowLevel {
            attribute :>> statement = "A space installed again on a processor after
                a shootdown of it began while it was installed elsewhere shall be
                translated through its tables as they are, never through a
                translation the shootdown took down.";
            attribute :>> criterion = "A page moved to a new frame while its space
                is not installed is read, after the switch back, as the new
                frame's marker: decisive on PCID hardware (the N150), where a kept
                tag would read the old frame.";
            attribute :>> parent = "H.MEM.17";
            attribute :>> unit = ("user::space::AddressSpace::install",
                "user::space::AddressSpace::uninstall");
        }
        // The parent is the requirement whose quantity the report counts: the
        // frames a space holds, as L.object.189 takes the cache's. No
        // high-level requirement covers /proc's reports as such.
        requirement <'L.user.171'> theResidentSetSplitsByKind : ItemLowLevel {
            attribute :>> statement = "/proc/<pid>/status shall report VmRSS, RssAnon,
                RssFile and RssShmem from each region's committed pages inside its own
                range: private anonymous memory as RssAnon, shared anonymous memory
                and a memory filesystem's file as RssShmem, a page a private file
                mapping has copied once, as RssAnon and not as its file's, and VmRSS
                their sum; /proc/<pid>/stat's rss shall be VmRSS in pages.";
            attribute :>> criterion = "In the procfs check process, VmRSS equals the
                three parts; two stack pages written for the first time add exactly 8
                kB to RssAnon and nothing to the others; one page of a shared
                anonymous mapping written adds exactly 4 kB to RssShmem alone; a page
                of a tmpfs file mapped shared adds 4 kB to RssShmem alone, and mapped
                shared twice 8 kB; a tmpfs file of eight pages
                with pages 0, 1 and 7 written, mapped as pages 0 and 2 to 7, adds 8 kB
                and not the page in the gap; mapped
                private and then written, it moves 4 kB from RssShmem to RssAnon with
                VmRSS unchanged; stat's field 24 times 4 kB is VmRSS.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("user::space::AddressSpace::resident_split",
                "user::space::Resident::total",
                "user::space::Resident::add",
                "user::space::Resident::take",
                "user::space::Counted::kind",
                "user::space::AddressSpace::list_counted",
                "user::space::count_object",
                "user::vmo::Vmo::committed_in",
                "user::vmo::Vmo::committed_in_both");
        }
        // As L.user.171's, the parent is the frames a space holds.
        requirement <'L.user.172'> aDiskFilesMappedPagesAreItsRss : ItemLowLevel {
            attribute :>> statement = "A disk file's pages inside a region's range shall
                count as RssFile, the file's other cached pages not at all, and a page
                a private mapping of it has written as RssAnon instead.";
            attribute :>> criterion = "With an eight-page file on the stage 12 disk all
                in the cache, a read-only mapping of pages 2 to 5 counts exactly 4
                file pages and no others; a private mapping of page 0, written,
                counts 1 anonymous page and no file page.";
            attribute :>> parent = "H.MEM.9";
            attribute :>> unit = ("user::space::AddressSpace::resident_split",
                "user::vmo::Vmo::is_disk_file");
        }
        // docs/RAM-RELEASE.md §3: the clean btrfs nodes, reclaimable heap.
        requirement <'L.user.175'> cleanBtrfsNodesAreAvailable : ItemLowLevel {
            attribute :>> statement = "/proc/meminfo's SReclaimable shall be the bytes the
                writable btrfs mounts' clean tree nodes hold, and MemAvailable shall
                count them besides the free frames and the clean page cache.";
            attribute :>> criterion = "With a wide directory read on the stage 12 disk,
                SReclaimable is the mounts' count in KiB, and MemAvailable exceeds
                the free frames and reclaimable pages by that count within half the
                volume's clean bytes (the `reclaim` line of test-btrfs).";
            attribute :>> parent = "H.QUOTA.12";
            attribute :>> unit = "ferrix_btrfs_write::volume::WriteVolume::clean_bytes";
        }
        // docs/RAM-RELEASE.md §2, R1 (consultant ledger 4971, 5022, 5038).
        requirement <'L.user.174'> devmgrStartsAgainFromTheKeptSet : ItemLowLevel {
            attribute :>> statement = "Under ferrix.devmgr=init every devmgr start
                after the first shall be made from the program, names and driver
                images the first start read, kept by the kernel, never from a path,
                and shall be refused when their fingerprint is not the one taken
                when they were kept.";
            attribute :>> criterion = "After the release, svc restart
                devmgr.service starts a second devmgr whose line says it came from
                the kept set, and / written, synced and read back afterwards answers
                what was written; a kept set whose stored fingerprint differs is
                refused with the line saying it is not as it was kept, and no
                second devmgr starts (test-init and its control).";
            attribute :>> parent = "H.DEV.4";
            attribute :>> unit = ("discovery::devmgr::read_files",
                "discovery::devmgr::read_set", "discovery::devmgr::fingerprint");
        }
    }
}
