/*
 * Ferrix — SysML v2 model, part 24: the low-level requirements of btrfs.
 *
 * What each unit of the two btrfs crates in the item does, as `ItemLowLevel`
 * requirements (part 13 defines the format, part 14 is the pilot this
 * copies): `ferrix-btrfs` (src/lib/fs/btrfs), the reader, and
 * `ferrix-btrfs-write` (src/lib/fs/btrfs-write), the write path. They
 * joined the item on 2026-10-02 (the customer's decision; ITEM.md). Their
 * interface below is the `Device` and `WriteDevice` traits, which the
 * kernel's block layer answers; above, `Volume` and `WriteVolume`, which the
 * VFS glue in the load (`ferrix-btrfs-vfs`, src/kernel/src/fs/btrfs*.rs)
 * calls. A unit is named from the crate's src/, led by the crate's name:
 * `ferrix_btrfs::volume::Volume::read_node`.
 *
 * The pilot's rules hold: a requirement names every function that carries
 * its behaviour as its `unit`; one check proves a whole requirement, so a
 * requirement is no broader than its checks can prove; a statement says no
 * more than its criterion tests; an accessor needs no requirement of its own.
 *
 * What proves what. Every check here is a host test of the crates
 * themselves, against images `mkfs.btrfs` made (src/lib/fs/btrfs/testdata)
 * and against a device in memory that records every write and flush, so
 * that a crash can be rebuilt from the record; except the two of L.btrfs.21,
 * which drive the write path through the load's `ferrix-btrfs-vfs` because
 * that is where a mount turns read-only, and the second half of L.btrfs.20,
 * which does too. The `btrfs_read` fuzzer (src/tests/fuzz) exercises the
 * reader beyond these and is not a verifier the gate reads. Nothing fuzzes
 * the write path.
 *
 * Written and not yet verified, so in the baseline: L.btrfs.9 and
 * L.btrfs.12, the volumes the reader and the writer refuse to open, which no
 * test opens; L.btrfs.22, fallible allocation in the writer, which the
 * branch btrfs-fallible is converting and whose tests will name it; and
 * L.btrfs.23, arithmetic that cannot overflow, which a lint would enforce
 * and none does yet (the certification consultant's advice, 2026-10-02).
 *
 * Ids are `L.btrfs.<n>`, numbered through both crates; the package gives the
 * part. They are never renumbered once a check names them.
 */
package FerrixBtrfsRequirements {
    private import FerrixItemRequirements::*;

    package Parsing {
        doc /* The reader's parsers: every structure read from a volume is
             * hostile input in ring 0, and every function is total over it
             * (the crate is `forbid(unsafe_code)`, reads every field through
             * `slice::get`, and checks every length sum). */

        requirement <'L.btrfs.1'> superblocksParsedOrRefused : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse, with an error
                naming what is wrong, a superblock that is truncated, has the wrong
                magic or a failing checksum, an unsupported checksum type, a sector or
                node size outside btrfs's range or a root no tree of that size can
                have, and every accessor of one it accepts shall answer.";
            attribute :>> criterion = "A well-formed superblock parses; a wrong magic,
                a flipped checksummed byte, a truncation, disagreeing sizes, an
                impossible root and checksum type 2 are each refused with their own
                error; each of 4,096 single-byte patches parses or is refused, and
                every accessor of what parses answers.";
            attribute :>> parent = ("H.STORE.1", "H.STORE.2");
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse",
                "ferrix_btrfs::superblock::Superblock::parse_at",
                "ferrix_btrfs::superblock::Superblock::check_roots",
                "ferrix_btrfs::verify_crc32c", "ferrix_btrfs::is_valid_block_size",
                "ferrix_btrfs::crc32c::crc32c_update");
        }
        requirement <'L.btrfs.2'> nodesParsedOrRefused : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a node that is
                truncated, fails its checksum, records another address than the one
                it was read from, is deeper than MAX_LEVEL, claims more items than its
                size holds, has an item outside the node or over the item array, has
                leaf payloads not packed back to back, or has keys out of order.";
            attribute :>> criterion = "A node from the wrong address, with a bad
                checksum, too many items, an item past its end, unpacked payloads,
                keys out of order, a level of 8 or a truncation is each refused with
                its own error; a leaf packed to its boundaries parses; each of 4,096
                single-byte patches of a leaf parses or is refused, and every accessor
                of what parses answers.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::Node::parse_unchecked_address",
                "ferrix_btrfs::tree::Node::check_capacity",
                "ferrix_btrfs::tree::Node::check_leaf",
                "ferrix_btrfs::tree::Node::check_internal",
                "ferrix_btrfs::tree::NodeHeader::parse",
                "ferrix_btrfs::tree::Node::search");
        }
        requirement <'L.btrfs.3'> chunksMappedOrRefused : ItemLowLevel {
            attribute :>> statement = "ChunkItem::parse and ChunkMap::insert shall
                refuse a chunk item btrfs would not write, one disagreeing with the
                volume's sector size, a non-system chunk in the system chunk array,
                and a chunk overlapping one already mapped; ChunkMap::insert shall
                answer ChunkMapFull rather than write past its storage; and map_copy
                shall map only the SINGLE, DUP and mirrored profiles, answering
                UnsupportedProfile for the others.";
            attribute :>> criterion = "A chunk with no stripes, a truncated one, one
                of a type, stripe count or size btrfs would not write, one of another
                sector size, a data chunk in the system array and an overlapping one
                are each refused; a map one entry short of a volume's chunks answers
                ChunkMapFull; a SINGLE chunk maps logical to physical, a mirrored one
                to each copy, and a striped one is refused; every layout btrfs writes
                still parses.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkItem::parse",
                "ferrix_btrfs::chunk::ChunkItem::check_type",
                "ferrix_btrfs::chunk::ChunkItem::check_geometry",
                "ferrix_btrfs::chunk::ChunkItem::check_sectorsize",
                "ferrix_btrfs::chunk::ChunkMap::insert",
                "ferrix_btrfs::chunk::ChunkMap::check_neighbours",
                "ferrix_btrfs::chunk::ChunkMap::insert_at",
                "ferrix_btrfs::chunk::ChunkMap::map_copy",
                "ferrix_btrfs::chunk::ChunkMap::load_sys_chunk_array",
                "ferrix_btrfs::chunk::SysChunkArray::entry");
        }
        requirement <'L.btrfs.4'> itemsParsedOrRefused : ItemLowLevel {
            attribute :>> statement = "The item parsers shall refuse an inode item,
                directory entry, inode reference or extent item that is truncated or
                that btrfs would not write, and the directory entry iterators shall
                stop, not overrun, at a name past the item's end.";
            attribute :>> criterion = "An inode item and extent data in all three
                forms decode; a truncated inode or extent item, an extent and a
                directory entry btrfs would not write, and an inode reference or
                extended reference Linux refuses are each refused; a directory item
                whose name runs past its end stops cleanly.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::InodeItem::parse",
                "ferrix_btrfs::items::InodeRefIter::record",
                "ferrix_btrfs::items::InodeExtrefIter::record",
                "ferrix_btrfs::items::DirItemIter::plausible",
                "ferrix_btrfs::items::DirItemIter::record",
                "ferrix_btrfs::items::ExtentData::parse",
                "ferrix_btrfs::items::ExtentData::extent",
                "ferrix_btrfs::items::ExtentData::parse_item",
                "ferrix_btrfs::items::ExtentData::check",
                "ferrix_btrfs::items::ExtentData::check_reference");
        }
        requirement <'L.btrfs.5'> decompressionStaysInItsBuffer : ItemLowLevel {
            attribute :>> statement = "compress::decompress shall answer
                BadCompressedData for a malformed zlib, LZO or zstd stream, a
                back-reference before the start of the output, or more output than
                the buffer holds, and shall write nothing outside its output
                buffer.";
            attribute :>> criterion = "Every truncation and every single-bit flip of
                real extents, random bodies behind valid headers, and outputs smaller
                than the content give an error or the oracle's decoding, for each of
                zlib, LZO and zstd; 0 panics.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::decompress",
                "ferrix_btrfs::compress::zlib::decompress",
                "ferrix_btrfs::compress::lzo::decompress",
                "ferrix_btrfs::compress::zstd::decompress");
        }
        requirement <'L.btrfs.6'> walksAscendAndEnd : ItemLowLevel {
            attribute :>> statement = "Volume::walk shall visit each key of a tree
                once, strictly ascending, and stop when its visitor breaks, and
                last_at_or_before shall find the last key at or before a key across
                leaf boundaries.";
            attribute :>> criterion = "A walk of every image visits each key once in
                order; a walk whose visitor breaks stops there; the last key at or
                before each probe is found across a leaf boundary; the predecessor
                steps through every field of a key.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::walk",
                "ferrix_btrfs::volume::Volume::seek",
                "ferrix_btrfs::volume::Volume::last_at_or_before",
                "ferrix_btrfs::volume::at_or_before",
                "ferrix_btrfs::volume::predecessor");
        }
        requirement <'L.btrfs.7'> namesChecked : ItemLowLevel {
            attribute :>> statement = "Subvolume::lookup and Subvolume::read_dir shall
                refuse a name no directory can hold, a name with a slash, and an
                entry filed under another name's hash, and shall answer a missing name
                as a miss, not an error.";
            attribute :>> criterion = "A lookup of an entry filed under another
                name's hash, or one no longer matching its hash, is refused; names no
                directory can hold, and a listing holding a name with a slash, are
                refused; a missing name is a miss.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::lookup",
                "ferrix_btrfs::fs::Subvolume::read_dir", "ferrix_btrfs::fs::check_name",
                "ferrix_btrfs::fs::target_of", "ferrix_btrfs::items::name_hash");
        }
        requirement <'L.btrfs.8'> fileReadsBounded : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall return a file's bytes as
                they were written, never more than were asked for nor past the
                inode's size, zeroes for a hole, and shall refuse an extent starting
                inside the one before it and a buffer smaller than an extent.";
            attribute :>> criterion = "The uncompressed, zlib, LZO and zstd images read
                back byte for byte; a read past the end returns nothing; a hole reads
                as zeroes; an overlapping extent and a too-small buffer are
                refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::Subvolume::copy_extent",
                "ferrix_btrfs::fs::ReadBuffers::new",
                "ferrix_btrfs::fs::ReadBuffers::expand_inline",
                "ferrix_btrfs::fs::ReadBuffers::expand_extent",
                "ferrix_btrfs::fs::Placement::new", "ferrix_btrfs::fs::expanded_len",
                "ferrix_btrfs::fs::copy_from");
        }
        requirement <'L.btrfs.9'> volumesTheReaderWouldMisreadRefused : ItemLowLevel {
            attribute :>> statement = "Volume::open shall refuse a volume setting an
                incompatible feature bit the reader does not know, one spanning more
                than one device, and one naming a log tree not yet replayed.";
            attribute :>> criterion = "Volume::open of an image with an unknown
                incompatible bit answers UnsupportedFeature, of one with two devices
                MultipleDevices, and of one with a log root UnreplayedLog.";
            attribute :>> parent = ("H.STORE.1", "H.STORE.4");
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::open",
                "ferrix_btrfs::volume::check_mountable");
        }
        requirement <'L.btrfs.23'> noArithmeticOverflows : ItemLowLevel {
            attribute :>> statement = "No addition, subtraction, multiplication or
                shift in ferrix-btrfs or ferrix-btrfs-write shall be able to overflow:
                each shall be checked, saturating or wrapping by intent, so that a
                value read from a volume can neither wrap silently in the release
                kernel, which has no overflow checks, nor panic one built with
                them.";
            attribute :>> criterion = "clippy's arithmetic_side_effects lint, denied
                for both crates' product code, reports no site. Not yet enforced: on
                2026-10-02 it reports 112, 56 in each crate.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::slice_at", "ferrix_btrfs::array_at",
                "ferrix_btrfs::tree::Node::check_leaf",
                "ferrix_btrfs::chunk::ChunkItem::check_geometry",
                "ferrix_btrfs::fs::Placement::new",
                "ferrix_btrfs_write::node::TreeNode::from_node");
        }
    }

    package Integrity {
        doc /* The reader's half of ASR-9: damage is found and not returned,
             * and a second copy is read where there is one. */

        requirement <'L.btrfs.10'> nodesReadFromAGoodCopy : ItemLowLevel {
            attribute :>> statement = "Volume::read_node shall try each copy of a node
                in turn and return the first whose checksum, address, level,
                generation and filesystem id match what its parent named, and
                otherwise the first copy's error.";
            attribute :>> criterion = "A corrupt tree node and a pointer to the wrong
                generation are refused, not misread, as is a tree deeper than btrfs
                allows; a device error propagates; a DUP node whose first copy fails
                is read from the second, and a volume whose root tree has one bad
                copy opens.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::read_node",
                "ferrix_btrfs::volume::Volume::read_node_copy",
                "ferrix_btrfs::volume::Volume::read_copy",
                "ferrix_btrfs::chunk::ChunkMap::copies");
        }
        requirement <'L.btrfs.11'> dataSectorsVerified : ItemLowLevel {
            attribute :>> statement = "A file read shall verify every data sector
                against the checksum tree before returning or decompressing it, read
                the other copy of a DUP sector that fails, and otherwise answer
                DataChecksum; a sector of a nodatasum file is read unchecked, as Linux
                reads it.";
            attribute :>> criterion = "A data sector that fails its checksum is an
                error, not bytes; a DUP sector whose first copy fails is read from the
                second; damage in a compressed extent fails its checksum before it is
                expanded; a nodatasum file reads back.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::fs::ReadBuffers::read_verified",
                "ferrix_btrfs::fs::read_checked", "ferrix_btrfs::fs::verify_sectors",
                "ferrix_btrfs::items::CsumItem::new",
                "ferrix_btrfs::items::CsumItem::checksum_for");
        }
    }

    package Commit {
        doc /* The write path's crash safety: copy-on-write, the order of a
             * commit's writes, and the log a fsync commits. */

        requirement <'L.btrfs.12'> volumesTheWriterCannotKeepRefused : ItemLowLevel {
            attribute :>> statement = "WriteVolume::open shall refuse with Unsupported
                a volume of more than one device, of a profile other than SINGLE or
                DUP, without a valid free-space tree, with non-skinny metadata,
                without NO_HOLES, with mixed groups, an unknown read-only-compatible
                feature, a subvolume, quotas or a shared tree block.";
            attribute :>> criterion = "WriteVolume::open of an image with each of these
                answers Unsupported naming it, and the device has no write.";
            attribute :>> parent = ("H.STORE.3", "H.STORE.5");
            attribute :>> unit = ("ferrix_btrfs_write::open::check_writable",
                "ferrix_btrfs_write::open::WriteVolume::open_committed",
                "ferrix_btrfs_write::open::WriteVolume::load_roots",
                "ferrix_btrfs_write::open::WriteVolume::load_block_groups",
                "ferrix_btrfs_write::open::WriteVolume::load_free_space");
        }
        requirement <'L.btrfs.13'> superblockLast : ItemLowLevel {
            attribute :>> statement = "WriteVolume::commit shall write every node the
                transaction owns to each of its copies, flush, write the primary
                superblock with write_durable, then the mirrors, and write no node
                after the primary superblock.";
            attribute :>> criterion = "In the device's record of a commit, a flush
                directly precedes the primary superblock, a flush directly follows it,
                and no node write comes after it; the commit's writes replayed without
                its superblocks leave a volume that checks clean and holds the last
                commit's items.";
            attribute :>> parent = "H.STORE.3";
            attribute :>> unit = ("ferrix_btrfs_write::commit::WriteVolume::commit",
                "ferrix_btrfs_write::commit::WriteVolume::write_nodes",
                "ferrix_btrfs_write::commit::WriteVolume::write_superblocks",
                "ferrix_btrfs_write::commit::WriteVolume::build_superblock",
                "ferrix_btrfs_write::commit::WriteVolume::write_backup_root",
                "ferrix_btrfs_write::commit::WriteVolume::finish_commit",
                "ferrix_btrfs_write::commit::sealed");
        }
        requirement <'L.btrfs.14'> committedBlocksNeverOverwritten : ItemLowLevel {
            attribute :>> statement = "An edit shall copy a node the last commit
                reaches to a newly allocated block before changing it, and a block
                freed in a transaction that the last commit uses shall stay pinned,
                unallocatable, until the commit is on the disk.";
            attribute :>> criterion = "Of 100 cuts over 4 random scenarios, each disk
                rebuilt as the device may have left it opens and checks clean; the
                commit's writes without its superblocks leave the last commit
                whole.";
            attribute :>> parent = ("H.STORE.3", "H.STORE.4");
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::cow",
                "ferrix_btrfs_write::volume::WriteVolume::cow_root",
                "ferrix_btrfs_write::volume::WriteVolume::cow_child",
                "ferrix_btrfs_write::volume::WriteVolume::free_node",
                "ferrix_btrfs_write::volume::WriteVolume::alloc_node",
                "ferrix_btrfs_write::space::Space::release",
                "ferrix_btrfs_write::space::Space::unpin",
                "ferrix_btrfs_write::space::BlockGroup::give_back");
        }
        requirement <'L.btrfs.15'> aLogKeepsItsPromise : ItemLowLevel {
            attribute :>> statement = "commit_log shall make an inode's logged items
                durable in a log tree, flushing before the superblock that names it,
                and opening a volume whose superblock names a log shall replay it
                before anything is allocated, keeping the later of two logs of one
                inode.";
            attribute :>> criterion = "A write logged and never committed is read back
                after the crash; the log commit's record has a flush before its
                superblock; its writes without the superblock leave the last commit;
                the later of two logs wins; an extent cut in two replays as one
                extent referred to twice; a commit after a log leaves no log.";
            attribute :>> parent = "H.STORE.4";
            attribute :>> unit = ("ferrix_btrfs_write::log::WriteVolume::log_inode",
                "ferrix_btrfs_write::log::WriteVolume::commit_log",
                "ferrix_btrfs_write::log::WriteVolume::seal_log",
                "ferrix_btrfs_write::log::WriteVolume::drop_log",
                "ferrix_btrfs_write::log::WriteVolume::replay_log",
                "ferrix_btrfs_write::log::WriteVolume::replay_extent",
                "ferrix_btrfs_write::log::WriteVolume::replay_sums",
                "ferrix_btrfs_write::log::logged_extent");
        }
    }

    package Space {
        doc /* The write path's allocator and the bookkeeping trees that
             * record what it handed out. */

        requirement <'L.btrfs.16'> superblockStripesExcluded : ItemLowLevel {
            attribute :>> statement = "Each block group shall keep every stripe a
                superblock copy lies in out of allocation, as Linux's
                exclude_super_stripes does, and the commit shall still write those
                stripes to the free-space tree as free.";
            attribute :>> criterion = "On a volume whose metadata chunk covers the
                64 MiB mirror, and one grown until a data chunk does, no tree block or
                data extent is allocated over the mirror's stripe, the volume checks
                clean, and the free-space tree lists the stripe free.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::chunks::Chunk::superblock_stripes",
                "ferrix_btrfs_write::space::BlockGroup::new",
                "ferrix_btrfs_write::commit::WriteVolume::sync_free_space_tree",
                "ferrix_btrfs_write::commit::WriteVolume::write_group_free_space");
        }
        requirement <'L.btrfs.17'> bookkeepingSettles : ItemLowLevel {
            attribute :>> statement = "Before writing, the commit shall turn delayed
                references into extent items and rewrite block-group items, the
                free-space tree and root items until a pass changes nothing, and
                answer Inconsistent if 64 passes do not settle; a reference count shall
                never go below zero.";
            attribute :>> criterion = "After 3,000 random edits on a blank and on a
                populated volume, and after building, editing, churning and deleting
                everything added, the writer's checker finds the extent tree, usage,
                free space and totals consistent and the reader reads back every item;
                a reference count taken below zero is refused.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::commit::WriteVolume::settle",
                "ferrix_btrfs_write::commit::WriteVolume::run_head",
                "ferrix_btrfs_write::commit::WriteVolume::write_block_group_items",
                "ferrix_btrfs_write::commit::WriteVolume::write_root_items",
                "ferrix_btrfs_write::commit::check_main_key",
                "ferrix_btrfs_write::extent::ExtentRecord::apply",
                "ferrix_btrfs_write::extent::ExtentRecord::to_items",
                "ferrix_btrfs_write::refs::DelayedRefs::add",
                "ferrix_btrfs_write::space::Space::account");
        }
        requirement <'L.btrfs.18'> rangeSetsExact : ItemLowLevel {
            attribute :>> statement = "A RangeSet shall merge touching runs on insert,
                split the run a removal falls in, take exactly the shared bytes on
                extract, and first_fit and first_prefix shall return only space the
                set holds, aligned and not crossing the boundary asked for.";
            attribute :>> criterion = "Inserts merge touching runs, removes split, add
                unions overlapping runs, extract takes exactly the shared bytes,
                first_fit aligns and avoids the boundary, and first_prefix takes what
                the run has.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::insert",
                "ferrix_btrfs_write::ranges::RangeSet::remove",
                "ferrix_btrfs_write::ranges::RangeSet::add",
                "ferrix_btrfs_write::ranges::RangeSet::extract",
                "ferrix_btrfs_write::ranges::RangeSet::first_fit",
                "ferrix_btrfs_write::ranges::RangeSet::first_prefix",
                "ferrix_btrfs_write::ranges::fit_in");
        }
        requirement <'L.btrfs.19'> everyCopyGetsAStripe : ItemLowLevel {
            attribute :>> statement = "A new chunk shall be placed only in the
                device's unallocated holes, with a stripe for every copy its profile
                needs, or refused with NoSpace before anything is recorded.";
            attribute :>> criterion = "In one 8 MiB hole a DUP chunk of 8 MiB is
                refused and two copies of 4 MiB are placed; data extents taken until
                the device has no room end in NoSpace, with the transaction not
                aborted.";
            attribute :>> parent = ("H.STORE.5", "H.STORE.6");
            attribute :>> unit = ("ferrix_btrfs_write::grow::place_stripes",
                "ferrix_btrfs_write::grow::WriteVolume::place_chunk",
                "ferrix_btrfs_write::grow::WriteVolume::device_holes",
                "ferrix_btrfs_write::volume::WriteVolume::grow");
        }
    }

    package Full {
        doc /* A volume that runs out, a transaction that fails, and an
             * allocation that is refused. */

        requirement <'L.btrfs.20'> refusedBeforeTheFirstEdit : ItemLowLevel {
            attribute :>> statement = "An operation shall be admitted only if the
                metadata room holds its own worst case, the commit's reserve and,
                unless it frees space, the global reserve, and a data write only if
                the data room holds it; otherwise it shall be refused with NoSpace
                before its first edit, aborting nothing.";
            attribute :>> criterion = "A write one sector past the data room is
                refused and one of exactly the room fits; creates until the trees are
                full end in NoSpace, the commit succeeds, and deleting 64 files makes
                room for another; a data write on full trees and one that needs a new
                metadata chunk abort nothing; through a mount, a full volume answers
                ENOSPC to the byte statfs offered and stays writable.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::admit",
                "ferrix_btrfs_write::volume::WriteVolume::commit_reserve",
                "ferrix_btrfs_write::volume::WriteVolume::ensure_space",
                "ferrix_btrfs_write::volume::WriteVolume::alloc_data",
                "ferrix_btrfs_write::grow::WriteVolume::data_room",
                "ferrix_btrfs_write::grow::WriteVolume::meta_room",
                "ferrix_btrfs_write::grow::WriteVolume::tree_headroom",
                "ferrix_btrfs_write::grow::WriteVolume::unallocated");
        }
        requirement <'L.btrfs.21'> anAbortedTransactionIsDropped : ItemLowLevel {
            attribute :>> statement = "A failure after an edit changed something shall
                mark the transaction aborted, and reload_read_only shall replace it
                with the last commit read from the device and answer ReadOnly to every
                edit after it, or, when the last commit cannot be read, leave the
                volume aborted.";
            attribute :>> criterion = "Through a mount, a commit whose device write
                fails answers EIO; the mount then reads the last commit, has no trace
                of the aborted transaction, answers EROFS to a write, a truncation
                and a create, and the disk holds the last commit; with reads failing
                as well, every lookup answers EIO and the mount says so once.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::abort_with",
                "ferrix_btrfs_write::volume::WriteVolume::check_writable",
                "ferrix_btrfs_write::volume::WriteVolume::guarded_with",
                "ferrix_btrfs_write::volume::WriteVolume::operation_with",
                "ferrix_btrfs_write::open::WriteVolume::reload_read_only",
                "ferrix_btrfs_write::open::WriteVolume::abort");
        }
        requirement <'L.btrfs.22'> allocationsAreFallible : ItemLowLevel {
            attribute :>> statement = "Every heap allocation of the write path shall be
                made fallibly, and a refused one shall be answered as an error that
                aborts the transaction if an edit had begun, never as a panic.";
            attribute :>> criterion = "With every allocation of a create, a data
                write, a commit and an open failed in turn, each answers an error or
                succeeds, no panic is taken, and the volume reopened is the last
                commit and checks clean.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::load",
                "ferrix_btrfs_write::volume::WriteVolume::read_node",
                "ferrix_btrfs_write::volume::WriteVolume::new_node",
                "ferrix_btrfs_write::node::TreeNode::from_node",
                "ferrix_btrfs_write::commit::WriteVolume::write_nodes");
        }
    }
}
