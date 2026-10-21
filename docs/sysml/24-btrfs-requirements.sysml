/*
 * Ferrix — SysML v2 model, part 24: the low-level requirements of btrfs.
 *
 * What each unit of the two btrfs crates in the item does, as `ItemLowLevel`
 * requirements (part 13 defines the format, part 14 is the pilot this
 * copies): `ferrix-btrfs` (src/lib/fs/btrfs/format), the reader, and
 * `ferrix-btrfs-write` (src/lib/fs/btrfs/write), the write path. They
 * joined the item on 2026-10-02 (the customer's decision; ITEM.md). Their
 * interface below is the `Device` and `WriteDevice` traits, which the
 * kernel's block layer answers; since branch fs-traits (docs/FS-COMMON.md,
 * the consultant's OK IF of 2026-10-19, ledger line 2587) those are a third
 * item crate's, `ferrix-fs-device` (src/lib/fs/common/device), which every
 * on-disk filesystem shares and which each btrfs crate re-exports, turning
 * a device's failure into its own error in one function (L.btrfs.117,
 * L.btrfs.118). Above, `Volume` and `WriteVolume`, which the
 * VFS glue in the load (`ferrix-btrfs-vfs`, src/kernel/src/fs/btrfs*.rs)
 * calls. A unit is named from the crate's src/, led by the crate's name:
 * `ferrix_btrfs::volume::Volume::read_node`.
 *
 * The pilot's rules hold: a requirement names every function that carries
 * its behaviour as its `unit`; one check proves a whole requirement, so a
 * requirement is no broader than one check can prove; a statement says no
 * more than its criterion tests; an accessor needs no requirement of its own.
 * The first version of this part, L.btrfs.1 to 23, broke the second rule:
 * many of them were conjunctions whose clauses different tests proved. The
 * certification consultant found it (2026-10-02). Each was narrowed to what
 * one test proves, and every clause it gave up that a test proves became a
 * requirement of its own, L.btrfs.24 to 107; ids a check named were not
 * renumbered. Clauses no test proved were dropped rather than kept
 * unverified: the commit's settling bound (64 passes), and a reload whose
 * own log replay aborts (docs/BACKLOG.md).
 *
 * What proves what. Every check here is a host test of the crates
 * themselves, against images `mkfs.btrfs` made (src/lib/fs/btrfs/format/testdata)
 * and against a device in memory that records every write and flush, so
 * that a crash can be rebuilt from the record; except L.btrfs.21, 105, 106
 * and 107, whose tests drive the write path through the load's
 * `ferrix-btrfs-vfs`, because a mount is where ENOSPC and a read-only
 * reload show. The `btrfs_read` fuzzer (src/tests/fuzz) exercises the
 * reader beyond these and is not a verifier the gate reads. Nothing fuzzes
 * the write path.
 *
 * Written and not yet verified, so in the baseline: L.btrfs.9 and
 * L.btrfs.12, the volumes the reader and the writer refuse to open, which no
 * test opens; and L.btrfs.23, arithmetic that cannot overflow, which a lint would enforce
 * and none does yet (the certification consultant's advice, 2026-10-02).
 *
 * Ids are `L.btrfs.<n>`, numbered through both crates; the package gives the
 * part. They are never renumbered once a check names them.
 */
package FerrixBtrfsRequirements {
    private import FerrixItemRequirements::*;

    package Superblocks {
        doc /* The superblock: the one structure read before anything is known
             * about the volume. */

        requirement <'L.btrfs.1'> superblockParsingIsTotal : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall answer any bytes with a
                superblock or an error, and every accessor of a superblock it returns
                shall answer without a panic.";
            attribute :>> criterion = "Each of 4,096 single-byte patches of a
                well-formed superblock (the first 1,024 bytes, each set to 0x00, 0x01,
                0x7F and 0xFF) parses or is refused, and every accessor of each that
                parses answers.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse",
                "ferrix_btrfs::superblock::Superblock::parse_at",
                "ferrix_btrfs::superblock::Superblock::check_roots",
                "ferrix_btrfs::is_valid_block_size");
        }
        requirement <'L.btrfs.24'> superblockChecksumChecked : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse a superblock whose
                CRC-32C does not match its bytes, with BadChecksum.";
            attribute :>> criterion = "A superblock with one checksummed byte changed is
                refused with BadChecksum.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse",
                "ferrix_btrfs::verify_crc32c");
        }
        requirement <'L.btrfs.25'> superblockMagicChecked : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse bytes without the
                btrfs magic, with BadMagic.";
            attribute :>> criterion = "A superblock with its magic changed is refused
                with BadMagic.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse");
        }
        requirement <'L.btrfs.26'> truncatedSuperblockRefused : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse a buffer shorter
                than a superblock, with Truncated.";
            attribute :>> criterion = "A superblock cut short is refused with Truncated
                naming the bytes needed and found.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse",
                "ferrix_btrfs::truncated");
        }
        requirement <'L.btrfs.27'> superblockSizesChecked : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse a sector, node or
                leaf size btrfs would not write, with BadSectorSize or BadNodeSize
                naming it.";
            attribute :>> criterion = "Each combination of sector, node and leaf size
                the test lists -- nodes smaller than a sector among them -- is refused
                with the error naming the size.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse",
                "ferrix_btrfs::is_valid_block_size");
        }
        requirement <'L.btrfs.28'> superblockRootsChecked : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse a superblock
                naming a root, a chunk root or a log root no tree can have.";
            attribute :>> criterion = "A root and a chunk root of level 8, a log root of
                level 255 and each other root the test lists are refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse",
                "ferrix_btrfs::superblock::Superblock::check_roots");
        }
        requirement <'L.btrfs.29'> unsupportedChecksumReported : ItemLowLevel {
            attribute :>> statement = "Superblock::parse shall refuse a superblock whose
                checksum type is not CRC-32C, with UnsupportedChecksum carrying the
                type.";
            attribute :>> criterion = "A superblock of checksum type 2 is refused with
                UnsupportedChecksum(2).";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::superblock::Superblock::parse");
        }
        requirement <'L.btrfs.30'> crc32cIsCastagnoli : ItemLowLevel {
            attribute :>> statement = "crc32c shall compute the CRC-32C (Castagnoli) of
                its input.";
            attribute :>> criterion = "crc32c of each published test vector equals its
                published value.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::crc32c::crc32c",
                "ferrix_btrfs::crc32c::crc32c_update");
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
                shift in ferrix-btrfs, ferrix-btrfs-write or ferrix-fs-device shall be
                able to overflow: each shall be checked, saturating or wrapping by
                intent, so that a value read from a volume can neither wrap silently
                in the release kernel, which has no overflow checks, nor panic one
                built with them.";
            attribute :>> criterion = "clippy's arithmetic_side_effects lint, denied
                for the product code of ferrix-btrfs, ferrix-btrfs-write and
                ferrix-fs-device, reports no site. Not yet enforced: on 2026-10-02 it
                reports 112, 56 in each btrfs crate; ferrix-fs-device has no
                arithmetic.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::slice_at", "ferrix_btrfs::array_at",
                "ferrix_btrfs::tree::Node::check_leaf",
                "ferrix_btrfs::chunk::ChunkItem::check_geometry",
                "ferrix_btrfs::fs::Placement::new",
                "ferrix_btrfs_write::node::TreeNode::from_node");
        }
    }

    package Nodes {
        doc /* Tree nodes, parsed before any field of them is used. */

        requirement <'L.btrfs.2'> nodeParsingIsTotal : ItemLowLevel {
            attribute :>> statement = "Node::parse shall answer any bytes with a node or
                an error, and every accessor of a node it returns shall answer without a
                panic.";
            attribute :>> criterion = "Each of 4,096 single-byte patches of a
                well-formed leaf parses or is refused, and every accessor of each that
                parses answers.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::Node::parse_unchecked_address",
                "ferrix_btrfs::tree::Node::check_capacity",
                "ferrix_btrfs::tree::Node::check_leaf",
                "ferrix_btrfs::tree::Node::check_internal",
                "ferrix_btrfs::tree::NodeHeader::parse");
        }
        requirement <'L.btrfs.31'> nodeAddressChecked : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a node that records
                another address than the one it was read from, with WrongAddress.";
            attribute :>> criterion = "A node parsed at another address than its own is
                refused with WrongAddress naming both.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse");
        }
        requirement <'L.btrfs.32'> nodeChecksumChecked : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a node whose CRC-32C
                does not match its bytes, with BadChecksum.";
            attribute :>> criterion = "A node with one checksummed byte changed is
                refused with BadChecksum.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::verify_crc32c");
        }
        requirement <'L.btrfs.33'> nodeItemCountChecked : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a node claiming more
                items than a node of its size holds, with TooManyItems.";
            attribute :>> criterion = "A node whose item count exceeds its capacity is
                refused with TooManyItems.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::Node::check_capacity");
        }
        requirement <'L.btrfs.34'> itemsStayInTheirNode : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a leaf with an item
                whose data lies outside the node or over the item array, with
                ItemOutOfBounds.";
            attribute :>> criterion = "A leaf with an item running past the node is
                refused with ItemOutOfBounds naming its slot.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::Node::check_leaf");
        }
        requirement <'L.btrfs.35'> keysInOrder : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a node whose keys are
                not strictly ascending, with ItemsOutOfOrder.";
            attribute :>> criterion = "A node with two keys out of order is refused with
                ItemsOutOfOrder naming the slot.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::Node::check_leaf",
                "ferrix_btrfs::tree::Node::check_internal");
        }
        requirement <'L.btrfs.36'> payloadsPacked : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a leaf whose item
                payloads are not packed back to back from the node's end.";
            attribute :>> criterion = "A leaf with a gap or an overlap between payloads
                is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::Node::check_leaf");
        }
        requirement <'L.btrfs.37'> nodeLevelBounded : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a node whose level
                exceeds MAX_LEVEL.";
            attribute :>> criterion = "A node of level 8 is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse_unchecked_address",
                "ferrix_btrfs::tree::NodeHeader::parse");
        }
        requirement <'L.btrfs.38'> truncatedNodeRefused : ItemLowLevel {
            attribute :>> statement = "Node::parse shall refuse a buffer shorter than
                the node it describes, with Truncated.";
            attribute :>> criterion = "A node cut short is refused with Truncated.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::parse",
                "ferrix_btrfs::tree::NodeHeader::parse");
        }
        requirement <'L.btrfs.39'> searchFindsOrPlaces : ItemLowLevel {
            attribute :>> statement = "Node::search shall return a present key's slot,
                and for an absent key the slot it would be inserted at.";
            attribute :>> criterion = "Present keys are found at their slots and absent
                ones answer the slot they would go in, as the test lists.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::tree::Node::search");
        }
    }

    package Chunks {
        doc /* Chunk items and the logical-to-physical map built from them. */

        requirement <'L.btrfs.3'> chunkMapNeverOverruns : ItemLowLevel {
            attribute :>> statement = "ChunkMap::insert shall answer ChunkMapFull, not
                write past its storage, when the map is full.";
            attribute :>> criterion = "A map one entry short of the chunks inserted
                answers ChunkMapFull for the last and keeps the others.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::insert",
                "ferrix_btrfs::chunk::ChunkMap::insert_at");
        }
        requirement <'L.btrfs.40'> overlappingChunksRefused : ItemLowLevel {
            attribute :>> statement = "ChunkMap::insert shall refuse a chunk whose range
                overlaps a different chunk already mapped, with BadChunk.";
            attribute :>> criterion = "A chunk overlapping one already mapped is refused
                with BadChunk.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::insert",
                "ferrix_btrfs::chunk::ChunkMap::check_neighbours");
        }
        requirement <'L.btrfs.41'> unsupportedProfilesRefused : ItemLowLevel {
            attribute :>> statement = "ChunkMap::map shall answer UnsupportedProfile for
                an address in a chunk of a striped or parity profile, never a guessed
                physical address.";
            attribute :>> criterion = "An address in a RAID0, RAID10, RAID5 or RAID6
                chunk is refused with UnsupportedProfile carrying its type bits.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::map_copy");
        }
        requirement <'L.btrfs.42'> singleChunksMap : ItemLowLevel {
            attribute :>> statement = "ChunkMap::map shall map an address in a SINGLE
                chunk to its stripe's offset plus the address's offset into the chunk.";
            attribute :>> criterion = "Addresses in a SINGLE chunk map to the physical
                addresses the test computes.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::map_copy");
        }
        requirement <'L.btrfs.43'> mirroredChunksMap : ItemLowLevel {
            attribute :>> statement = "ChunkMap::map shall map an address in a DUP or
                RAID1 chunk to the first stripe, and refuse one in a RAID0 chunk.";
            attribute :>> criterion = "A DUP and a RAID1 chunk map to their first
                stripe; a RAID0 chunk is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::map_copy");
        }
        requirement <'L.btrfs.44'> chunksBtrfsWouldNotWriteRefused : ItemLowLevel {
            attribute :>> statement = "ChunkItem::parse shall refuse a chunk item of a
                stripe length, length, type or stripe count btrfs would not write, with
                BadChunk.";
            attribute :>> criterion = "Each malformed chunk item the test lists is
                refused with BadChunk.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkItem::parse",
                "ferrix_btrfs::chunk::ChunkItem::check_type",
                "ferrix_btrfs::chunk::ChunkItem::check_geometry");
        }
        requirement <'L.btrfs.45'> chunksWithoutStripesRefused : ItemLowLevel {
            attribute :>> statement = "ChunkItem::parse shall refuse a chunk item with
                no stripes.";
            attribute :>> criterion = "A chunk item with num_stripes zero is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkItem::parse",
                "ferrix_btrfs::chunk::ChunkItem::check_geometry");
        }
        requirement <'L.btrfs.46'> truncatedChunksRefused : ItemLowLevel {
            attribute :>> statement = "ChunkItem::parse shall refuse a chunk item
                shorter than its stripe records.";
            attribute :>> criterion = "A chunk item cut inside its stripe records is
                refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkItem::parse");
        }
        requirement <'L.btrfs.47'> chunkSectorSizeChecked : ItemLowLevel {
            attribute :>> statement = "A chunk shall be refused when its sector size
                differs from the volume's or its start is off the sector grid.";
            attribute :>> criterion = "A chunk of another sector size, and one starting
                off the grid, are refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkItem::check_sectorsize");
        }
        requirement <'L.btrfs.48'> systemArrayHoldsSystemChunks : ItemLowLevel {
            attribute :>> statement = "The system chunk array shall be refused when it
                holds a chunk that is not a system chunk.";
            attribute :>> criterion = "A system chunk array holding a data chunk is
                refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::load_sys_chunk_array",
                "ferrix_btrfs::chunk::SysChunkArray::entry");
        }
        requirement <'L.btrfs.49'> systemArrayBootstrapsTheMap : ItemLowLevel {
            attribute :>> statement = "ChunkMap::load_sys_chunk_array shall map every
                chunk the superblock's system chunk array holds.";
            attribute :>> criterion = "Every chunk of the system chunk array the test
                builds is mapped after the load.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::chunk::ChunkMap::load_sys_chunk_array",
                "ferrix_btrfs::chunk::SysChunkArray::entry");
        }
    }

    package Items {
        doc /* Item payloads, parsed for their key type. */

        requirement <'L.btrfs.4'> directoryEntriesStopAtTheEnd : ItemLowLevel {
            attribute :>> statement = "Iterating a directory item shall stop, not read
                past the item, at an entry whose name runs past the item's end.";
            attribute :>> criterion = "A directory item whose last name runs past its
                end yields the entries before it and then stops.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::DirItemIter::record",
                "ferrix_btrfs::items::DirItemIter::plausible");
        }
        requirement <'L.btrfs.50'> truncatedInodeItemsRefused : ItemLowLevel {
            attribute :>> statement = "InodeItem::parse shall refuse a payload shorter
                than an inode item.";
            attribute :>> criterion = "An inode item cut short is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::InodeItem::parse");
        }
        requirement <'L.btrfs.51'> directoryEntriesBtrfsWouldNotWriteRefused : ItemLowLevel {
            attribute :>> statement = "Iterating a directory item shall refuse an entry
                btrfs would not write: an empty or over-long name, an unknown type, or
                data where none belongs.";
            attribute :>> criterion = "Each malformed entry the test lists is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::DirItemIter::record",
                "ferrix_btrfs::items::DirItemIter::plausible");
        }
        requirement <'L.btrfs.52'> inodeRefNamesBounded : ItemLowLevel {
            attribute :>> statement = "Iterating an inode reference shall accept names
                of 1 to 255 bytes and refuse others.";
            attribute :>> criterion = "Names of 1 and 255 bytes are read, and names
                outside that length are refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::InodeRefIter::record");
        }
        requirement <'L.btrfs.53'> truncatedExtentItemsRefused : ItemLowLevel {
            attribute :>> statement = "ExtentData::parse shall refuse a payload shorter
                than its form of extent item.";
            attribute :>> criterion = "The truncated extent items the test lists are
                refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::ExtentData::parse",
                "ferrix_btrfs::items::ExtentData::extent");
        }
        requirement <'L.btrfs.54'> extentsBtrfsWouldNotWriteRefused : ItemLowLevel {
            attribute :>> statement = "ExtentData::parse_item shall refuse an extent
                item btrfs would not write: an unknown type, a range outside its extent,
                or a misaligned offset.";
            attribute :>> criterion = "Each malformed extent the test lists is refused
                with BadItem.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::ExtentData::parse_item",
                "ferrix_btrfs::items::ExtentData::check",
                "ferrix_btrfs::items::ExtentData::check_reference");
        }
        requirement <'L.btrfs.55'> extrefsLinuxRefusesRefused : ItemLowLevel {
            attribute :>> statement = "Iterating an inode extended reference shall
                refuse the records Linux refuses.";
            attribute :>> criterion = "Well-formed extended references parse, and each
                the test lists that Linux refuses is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::items::InodeExtrefIter::record");
        }
    }

    package Compression {
        doc /* The three decoders of compressed extents. */

        requirement <'L.btrfs.5'> zlibIsTotalUnderBitFlips : ItemLowLevel {
            attribute :>> statement = "zlib::decompress shall answer every stream with
                an error or the bytes it encodes, within its output buffer.";
            attribute :>> criterion = "Every single-bit flip of real zlib extents
                decodes to what miniz_oxide decodes or is an error; 0 panics.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zlib::decompress",
                "ferrix_btrfs::compress::decompress");
        }
        requirement <'L.btrfs.56'> zlibTruncationsRefused : ItemLowLevel {
            attribute :>> statement = "zlib::decompress shall refuse a stream cut before
                its end.";
            attribute :>> criterion = "Every truncation of a real extent before its
                trailer is an error.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zlib::decompress");
        }
        requirement <'L.btrfs.57'> zlibOutputBounded : ItemLowLevel {
            attribute :>> statement = "zlib::decompress shall refuse a stream that
                decodes to more than its output buffer holds.";
            attribute :>> criterion = "A stream decoding past a smaller output buffer is
                an error, and nothing is written past the buffer.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zlib::decompress");
        }
        requirement <'L.btrfs.58'> zlibDistancesBounded : ItemLowLevel {
            attribute :>> statement = "zlib::decompress shall refuse a back-reference
                before the start of its output.";
            attribute :>> criterion = "A stream whose first match reaches before the
                output's start is an error.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zlib::decompress");
        }
        requirement <'L.btrfs.59'> lzoIsTotalUnderBitFlips : ItemLowLevel {
            attribute :>> statement = "lzo::decompress shall answer every stream with an
                error or decoded bytes, within its output buffer.";
            attribute :>> criterion = "Every single-bit flip of real LZO extents
                answers; 0 panics.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::lzo::decompress",
                "ferrix_btrfs::compress::decompress");
        }
        requirement <'L.btrfs.60'> lzoTruncationsAnswered : ItemLowLevel {
            attribute :>> statement = "lzo::decompress shall answer every truncated
                stream without a panic.";
            attribute :>> criterion = "Every truncation of a real LZO extent answers.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::lzo::decompress");
        }
        requirement <'L.btrfs.61'> lzoOutputBounded : ItemLowLevel {
            attribute :>> statement = "lzo::decompress shall decode no more than its
                output buffer holds.";
            attribute :>> criterion = "A stream decoded into a smaller output buffer
                stops at the buffer and writes nothing past it.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::lzo::decompress");
        }
        requirement <'L.btrfs.62'> zstdIsTotalUnderBitFlips : ItemLowLevel {
            attribute :>> statement = "zstd::decompress shall answer every stream with
                an error or decoded bytes, within its output buffer.";
            attribute :>> criterion = "Every single-bit flip of real zstd frames
                answers; 0 panics.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zstd::decompress",
                "ferrix_btrfs::compress::decompress");
        }
        requirement <'L.btrfs.63'> zstdTruncationsRefused : ItemLowLevel {
            attribute :>> statement = "zstd::decompress shall refuse a frame cut short
                as corrupt.";
            attribute :>> criterion = "Every truncation of a real zstd frame is
                BadCompressedData.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zstd::decompress");
        }
        requirement <'L.btrfs.64'> zstdOutputBounded : ItemLowLevel {
            attribute :>> statement = "zstd::decompress shall refuse a frame whose
                content is larger than its output buffer.";
            attribute :>> criterion = "A frame decoded into a buffer smaller than its
                content is BadCompressedData.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zstd::decompress");
        }
        requirement <'L.btrfs.65'> zstdMatchesBounded : ItemLowLevel {
            attribute :>> statement = "zstd::decompress shall refuse a match before the
                start of its output.";
            attribute :>> criterion = "A frame whose first match reaches before the
                output's start is BadCompressedData.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::compress::zstd::decompress");
        }
    }

    package Walks {
        doc /* Searching and walking a tree. */

        requirement <'L.btrfs.6'> walksAscendOnce : ItemLowLevel {
            attribute :>> statement = "Volume::walk shall visit every key of a tree
                once, in strictly ascending order.";
            attribute :>> criterion = "A walk of each of the four images' fs trees
                visits every key once, each greater than the one before.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::walk",
                "ferrix_btrfs::volume::Volume::seek");
        }
        requirement <'L.btrfs.66'> walksStopWhenAsked : ItemLowLevel {
            attribute :>> statement = "Volume::walk shall stop at the key its visitor
                breaks on.";
            attribute :>> criterion = "A walk whose visitor breaks stops at that key.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::walk");
        }
        requirement <'L.btrfs.67'> lastAtOrBeforeAcrossLeaves : ItemLowLevel {
            attribute :>> statement = "Volume::last_at_or_before shall return the last
                key at or before the one asked, across leaf boundaries.";
            attribute :>> criterion = "For every pair of neighbouring keys of the four
                images, the later finds itself and a key in the gap finds the earlier.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::last_at_or_before",
                "ferrix_btrfs::volume::at_or_before");
        }
        requirement <'L.btrfs.68'> predecessorIsExact : ItemLowLevel {
            attribute :>> statement = "predecessor shall return the greatest key below
                its argument, through every field.";
            attribute :>> criterion = "predecessor steps back through each field of a
                key as the test lists.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::volume::predecessor");
        }
    }

    package NodeReads {
        doc /* Reading a node from the copy that passes its checks. */

        requirement <'L.btrfs.10'> nodesReadFromAGoodCopy : ItemLowLevel {
            attribute :>> statement = "Volume::read_node shall try each copy of a node
                in turn and return the first that passes its checks.";
            attribute :>> criterion = "With the first copy of a DUP node damaged, the
                read returns the node from the second.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::read_node",
                "ferrix_btrfs::volume::Volume::read_node_copy",
                "ferrix_btrfs::volume::Volume::read_copy",
                "ferrix_btrfs::chunk::ChunkMap::copies");
        }
        requirement <'L.btrfs.69'> corruptNodesRefused : ItemLowLevel {
            attribute :>> statement = "Volume::read_node shall refuse a node none of
                whose copies passes its checks, with the first copy's error.";
            attribute :>> criterion = "A tree node with a damaged byte, in a SINGLE
                chunk, is refused and not read as a node.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::read_node",
                "ferrix_btrfs::volume::Volume::read_node_copy");
        }
        requirement <'L.btrfs.70'> staleNodesRefused : ItemLowLevel {
            attribute :>> statement = "Volume::read_node shall refuse a node whose
                generation differs from what its parent named, with BadTree.";
            attribute :>> criterion = "A root pointer with its generation raised by one
                is refused with BadTree.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::read_node_copy");
        }
        requirement <'L.btrfs.71'> deviceErrorsPropagate : ItemLowLevel {
            attribute :>> statement = "A failed device read shall be answered as
                DeviceRead at its physical address, not as damage.";
            attribute :>> criterion = "A device failing the root's read answers
                DeviceRead at that address.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::read_copy",
                "ferrix_btrfs::volume::Volume::read_node",
                "ferrix_btrfs::volume::read_error",
                "ferrix_btrfs::volume::read_device");
        }
        requirement <'L.btrfs.117'> theReaderMapsEachDeviceFailure : ItemLowLevel {
            attribute :>> statement = "The reader shall answer a device read that failed
                for want of memory with OutOfMemory, as it is, and one that failed in
                any other way with DeviceRead at the physical address the reader asked
                for, whatever address the device reported.";
            attribute :>> criterion = "A device failing every read with each of
                ferrix-fs-device's four errors, each reporting an address other than the
                one asked, answers OutOfMemory for OutOfMemory and DeviceRead at the
                address asked for the other three, through read_error, through
                read_device and through Volume::open, at the primary superblock's
                offset.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::read_error",
                "ferrix_btrfs::volume::read_device");
        }
        requirement <'L.btrfs.72'> overDeepTreesRefused : ItemLowLevel {
            attribute :>> statement = "A tree whose root names a level above MAX_LEVEL
                shall be refused with BadTree before anything is read.";
            attribute :>> criterion = "A root of level 8 is refused with BadTree.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::seek",
                "ferrix_btrfs::volume::Volume::read_node_copy");
        }
    }

    package Files {
        doc /* Names and file contents, read through a subvolume. */

        requirement <'L.btrfs.7'> lookupChecksTheHash : ItemLowLevel {
            attribute :>> statement = "Subvolume::lookup shall refuse an entry filed
                under another name's hash.";
            attribute :>> criterion = "A lookup reaching an entry whose name hashes to
                another key is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::lookup",
                "ferrix_btrfs::items::name_hash");
        }
        requirement <'L.btrfs.73'> lookupChecksTheName : ItemLowLevel {
            attribute :>> statement = "Subvolume::lookup shall refuse a name that no
                longer matches its hash.";
            attribute :>> criterion = "A lookup of a name changed on the disk after it
                was filed is refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::lookup",
                "ferrix_btrfs::items::name_hash");
        }
        requirement <'L.btrfs.74'> impossibleNamesRefused : ItemLowLevel {
            attribute :>> statement = "check_name shall refuse an empty name, `.`, `..`
                and a name holding `/` or NUL, and accept other names of up to 255
                bytes; a lookup shall refuse the same names.";
            attribute :>> criterion = "Six names -- empty, `.`, `..`, `/`, `a/b` and one
                with a NUL -- are refused by check_name and by a lookup, and six others,
                `...` and one of 255 bytes among them, are accepted.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::check_name",
                "ferrix_btrfs::fs::entry_named");
        }
        requirement <'L.btrfs.75'> listingsCheckNames : ItemLowLevel {
            attribute :>> statement = "Subvolume::read_dir shall refuse a listing that
                holds a name with a slash.";
            attribute :>> criterion = "A listing whose entry's name holds a slash is
                refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read_dir",
                "ferrix_btrfs::fs::check_name", "ferrix_btrfs::fs::target_of");
        }
        requirement <'L.btrfs.76'> missingNamesMiss : ItemLowLevel {
            attribute :>> statement = "Subvolume::lookup shall answer a name the
                directory does not hold as a miss, not an error.";
            attribute :>> criterion = "A lookup of an absent name answers None.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::lookup");
        }
        requirement <'L.btrfs.8'> plainFilesReadBack : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall return an uncompressed
                file's bytes as they were written.";
            attribute :>> criterion = "Every file of the uncompressed image reads back
                byte for byte.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::Subvolume::copy_extent",
                "ferrix_btrfs::fs::Placement::new", "ferrix_btrfs::fs::copy_from");
        }
        requirement <'L.btrfs.77'> zlibFilesReadBack : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall return a zlib-compressed
                file's bytes as they were written.";
            attribute :>> criterion = "Every file of the zlib image reads back byte for
                byte.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::ReadBuffers::expand_extent",
                "ferrix_btrfs::fs::ReadBuffers::expand_inline",
                "ferrix_btrfs::fs::expanded_len");
        }
        requirement <'L.btrfs.78'> lzoFilesReadBack : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall return an LZO-compressed
                file's bytes as they were written.";
            attribute :>> criterion = "Every file of the LZO image reads back byte for
                byte.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::ReadBuffers::expand_extent",
                "ferrix_btrfs::fs::ReadBuffers::expand_inline",
                "ferrix_btrfs::fs::expanded_len");
        }
        requirement <'L.btrfs.79'> zstdFilesReadBack : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall return a zstd-compressed
                file's bytes as they were written.";
            attribute :>> criterion = "Every file of the zstd image reads back byte for
                byte.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::ReadBuffers::expand_extent",
                "ferrix_btrfs::fs::ReadBuffers::expand_inline",
                "ferrix_btrfs::fs::expanded_len");
        }
        requirement <'L.btrfs.80'> readsEndAtTheEnd : ItemLowLevel {
            attribute :>> statement = "Subvolume::read past a file's end shall return
                nothing, and Subvolume::read_dir shall resume a listing from the cursor
                it returned.";
            attribute :>> criterion = "A read past the end returns nothing, and a
                listing resumed from its cursor yields the rest.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::Subvolume::read_dir");
        }
        requirement <'L.btrfs.81'> holesReadAsZeroes : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall return zeroes for a hole.";
            attribute :>> criterion = "A file's hole reads as zeroes.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::Placement::new");
        }
        requirement <'L.btrfs.82'> overlappingExtentsRefused : ItemLowLevel {
            attribute :>> statement = "Subvolume::read shall refuse an extent that
                starts inside the one before it.";
            attribute :>> criterion = "An extent starting inside the one before it is
                refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::Subvolume::read",
                "ferrix_btrfs::fs::Placement::new");
        }
        requirement <'L.btrfs.83'> smallBuffersRefused : ItemLowLevel {
            attribute :>> statement = "ReadBuffers::new shall refuse buffers smaller
                than the largest extent they must hold.";
            attribute :>> criterion = "Buffers smaller than an extent are refused.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::ReadBuffers::new");
        }
    }

    package DataSectors {
        doc /* The reader's checksums of file data. */

        requirement <'L.btrfs.11'> damagedSectorsRefused : ItemLowLevel {
            attribute :>> statement = "A file read shall answer DataChecksum, and no
                bytes, for a data sector whose CRC-32C differs from the checksum
                tree's.";
            attribute :>> criterion = "A read of a file with one damaged data sector, in
                a SINGLE chunk, answers DataChecksum.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::fs::ReadBuffers::read_verified",
                "ferrix_btrfs::fs::read_checked", "ferrix_btrfs::fs::verify_sectors",
                "ferrix_btrfs::items::CsumItem::checksum_for");
        }
        requirement <'L.btrfs.84'> damagedSectorsReadFromTheOtherCopy : ItemLowLevel {
            attribute :>> statement = "A file read shall read a DUP data sector that
                fails its checksum from its other copy, and answer an error when both
                fail.";
            attribute :>> criterion = "With the first copy of a DUP sector damaged the
                read returns the file's bytes; with both damaged it answers
                DataChecksum.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::fs::read_checked",
                "ferrix_btrfs::fs::verify_sectors",
                "ferrix_btrfs::fs::ReadBuffers::read_verified");
        }
        requirement <'L.btrfs.85'> compressedExtentsCheckedFirst : ItemLowLevel {
            attribute :>> statement = "A compressed extent shall be checked against its
                checksums before it is expanded.";
            attribute :>> criterion = "Damage inside a compressed extent is
                DataChecksum, not BadCompressedData.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::fs::ReadBuffers::read_verified",
                "ferrix_btrfs::fs::ReadBuffers::expand_extent");
        }
        requirement <'L.btrfs.86'> nodatasumFilesReadUnchecked : ItemLowLevel {
            attribute :>> statement = "A file marked nodatasum shall be read without
                checksums, as Linux reads it.";
            attribute :>> criterion = "A nodatasum file reads back byte for byte.";
            attribute :>> parent = "H.STORE.1";
            attribute :>> unit = ("ferrix_btrfs::fs::ReadBuffers::read_verified",
                "ferrix_btrfs::fs::read_checked");
        }
    }

    package Commit {
        doc /* The write path's crash safety: copy-on-write and the order of a
             * commit's writes. */

        requirement <'L.btrfs.13'> superblockLast : ItemLowLevel {
            attribute :>> statement = "WriteVolume::commit shall flush directly before
                writing the primary superblock, make the primary durable before any
                mirror, and write no node after it.";
            attribute :>> criterion = "In the device's record of a commit a flush
                directly precedes and directly follows the primary superblock, and no
                node write comes after it.";
            attribute :>> parent = "H.STORE.3";
            // ferrix_fs_device::write_durable is WriteDevice's default method,
            // which the kernel's Disk does not override, so every commit's
            // primary superblock runs it. It is named without its trait as
            // owner because check-traceability.py gives a trait block none
            // (the consultant's condition 1, ledger line 2806; BACKLOG has
            // the resolver's row).
            attribute :>> unit = ("ferrix_btrfs_write::commit::WriteVolume::commit",
                "ferrix_btrfs_write::commit::WriteVolume::write_nodes",
                "ferrix_btrfs_write::commit::WriteVolume::write_superblocks",
                "ferrix_btrfs_write::commit::WriteVolume::finish_commit",
                "ferrix_btrfs_write::device::write_durable",
                "ferrix_fs_device::write_durable");
        }
        requirement <'L.btrfs.14'> committedBlocksNeverOverwritten : ItemLowLevel {
            attribute :>> statement = "No write a transaction makes before its
                superblock shall land on a block the last commit's trees reach.";
            attribute :>> criterion = "500 inserts committed, their writes replayed
                without the superblocks onto the volume as it was, leave a volume that
                checks clean and holds exactly the last commit's items.";
            attribute :>> parent = "H.STORE.3";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::cow",
                "ferrix_btrfs_write::volume::WriteVolume::cow_root",
                "ferrix_btrfs_write::volume::WriteVolume::cow_child",
                "ferrix_btrfs_write::volume::WriteVolume::free_node",
                "ferrix_btrfs_write::volume::WriteVolume::alloc_node",
                "ferrix_btrfs_write::space::Space::release",
                "ferrix_btrfs_write::space::Space::unpin",
                "ferrix_btrfs_write::space::BlockGroup::give_back");
        }
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
    }

    package Log {
        doc /* The log a fsync commits, and its replay at open. */

        requirement <'L.btrfs.15'> aLogKeepsItsPromise : ItemLowLevel {
            attribute :>> statement = "An inode's data logged with log_inode and
                commit_log shall be read back after a crash, the log replayed when the
                volume opens.";
            attribute :>> criterion = "A file rewritten, logged and never committed,
                then opened from the crashed device, holds the rewrite.";
            attribute :>> parent = "H.STORE.4";
            attribute :>> unit = ("ferrix_btrfs_write::log::WriteVolume::log_inode",
                "ferrix_btrfs_write::log::WriteVolume::commit_log",
                "ferrix_btrfs_write::log::WriteVolume::seal_log",
                "ferrix_btrfs_write::log::WriteVolume::replay_log",
                "ferrix_btrfs_write::log::WriteVolume::replay_extent",
                "ferrix_btrfs_write::log::WriteVolume::replay_sums",
                "ferrix_btrfs_write::open::WriteVolume::open_committed");
        }
        requirement <'L.btrfs.87'> commitsDropTheLog : ItemLowLevel {
            attribute :>> statement = "A commit shall carry what a log held into the
                trees and leave the superblock naming no log.";
            attribute :>> criterion = "After a log commit and then a commit, the
                superblock names no log and the file holds the logged data.";
            attribute :>> parent = "H.STORE.4";
            attribute :>> unit = ("ferrix_btrfs_write::commit::WriteVolume::commit",
                "ferrix_btrfs_write::log::WriteVolume::drop_log");
        }
        requirement <'L.btrfs.88'> logSuperblockLast : ItemLowLevel {
            attribute :>> statement = "commit_log shall flush directly before writing
                the superblock that names the log.";
            attribute :>> criterion = "In the device's record of a log commit a flush
                directly precedes the superblock.";
            attribute :>> parent = "H.STORE.3";
            attribute :>> unit = ("ferrix_btrfs_write::log::WriteVolume::commit_log",
                "ferrix_btrfs_write::log::WriteVolume::seal_log");
        }
        requirement <'L.btrfs.89'> unnamedLogsChangeNothing : ItemLowLevel {
            attribute :>> statement = "A log commit's writes without its superblock
                shall leave the volume at its last commit.";
            attribute :>> criterion = "The log commit's writes replayed without its
                superblock leave the file as last committed.";
            attribute :>> parent = "H.STORE.3";
            attribute :>> unit = ("ferrix_btrfs_write::log::WriteVolume::commit_log");
        }
        requirement <'L.btrfs.90'> theLaterLogWins : ItemLowLevel {
            attribute :>> statement = "Replaying a log of an inode logged twice shall
                leave the later of the two.";
            attribute :>> criterion = "An inode logged with one content and again with
                another opens holding the second.";
            attribute :>> parent = "H.STORE.4";
            attribute :>> unit = ("ferrix_btrfs_write::log::WriteVolume::log_inode",
                "ferrix_btrfs_write::log::WriteVolume::replay_log");
        }
        requirement <'L.btrfs.91'> cutExtentsReplayWhole : ItemLowLevel {
            attribute :>> statement = "Replaying a logged extent cut in two shall make
                one extent referred to twice, not two.";
            attribute :>> criterion = "A file whose extent an overwrite cut in two
                replays with one extent and two references to it, and checks clean.";
            attribute :>> parent = "H.STORE.4";
            attribute :>> unit = ("ferrix_btrfs_write::log::WriteVolume::replay_extent",
                "ferrix_btrfs_write::log::logged_extent");
        }
    }

    package Space {
        doc /* The write path's allocator and the bookkeeping trees that record
             * what it handed out. */

        requirement <'L.btrfs.16'> superblockStripesExcludedFromMetadata : ItemLowLevel {
            attribute :>> statement = "A metadata block group whose chunk covers a
                superblock mirror shall allocate no tree block in the mirror's stripe,
                and its free-space tree shall still list the stripe free.";
            attribute :>> criterion = "On the blank fixture, whose DUP metadata chunk
                covers the 64 MiB mirror, edits until past the mirror leave no tree
                block in its stripe, the volume checks clean, and the free-space tree
                lists the stripe free.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit =
                ("ferrix_btrfs_write::chunks::Chunk::superblock_stripes",
                "ferrix_btrfs_write::space::BlockGroup::new",
                "ferrix_btrfs_write::commit::WriteVolume::sync_free_space_tree",
                "ferrix_btrfs_write::commit::WriteVolume::write_group_free_space");
        }
        requirement <'L.btrfs.92'> superblockStripesExcludedFromData : ItemLowLevel {
            attribute :>> statement = "A data chunk made over a superblock mirror shall
                keep the mirror's stripe out of data allocation.";
            attribute :>> criterion = "On a volume grown until a data chunk covers the
                64 MiB mirror, every data extent then allocated lies outside the
                stripe.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit =
                ("ferrix_btrfs_write::chunks::Chunk::superblock_stripes",
                "ferrix_btrfs_write::space::BlockGroup::new",
                "ferrix_btrfs_write::space::Space::alloc_data");
        }
        requirement <'L.btrfs.17'> bookkeepingConsistent : ItemLowLevel {
            attribute :>> statement = "Each commit shall leave every tree block's and
                data extent's references, each block group's usage and free space, and
                the superblock's total consistent with the trees.";
            attribute :>> criterion = "After 3,000 random edits with commits between
                them, the writer's checker finds every count consistent and the reader
                reads back every item the model holds.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::commit::WriteVolume::settle",
                "ferrix_btrfs_write::commit::WriteVolume::run_head",
                "ferrix_btrfs_write::commit::WriteVolume::write_block_group_items",
                "ferrix_btrfs_write::commit::WriteVolume::write_root_items",
                "ferrix_btrfs_write::commit::check_main_key",
                "ferrix_btrfs_write::extent::ExtentRecord::to_items",
                "ferrix_btrfs_write::refs::DelayedRefs::add",
                "ferrix_btrfs_write::space::Space::account");
        }
        requirement <'L.btrfs.93'> treesShrinkBack : ItemLowLevel {
            attribute :>> statement = "Deleting every item added to a tree shall shrink
                it back to a leaf holding what it held before.";
            attribute :>> criterion = "2,000 items inserted, making a tree of three
                levels, then deleted, leave a tree of level 0 with the items it had,
                checking clean.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::tree::WriteVolume::delete",
                "ferrix_btrfs_write::tree::WriteVolume::shrink_root",
                "ferrix_btrfs_write::tree::WriteVolume::remove_ptr");
        }
        requirement <'L.btrfs.94'> countsNeverBelowZero : ItemLowLevel {
            attribute :>> statement = "ExtentRecord::apply shall refuse a reference
                count taken below zero, or above one for a tree block's single owner.";
            attribute :>> criterion = "Removing a reference never added, and adding one
                owner twice, are refused.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::extent::ExtentRecord::apply");
        }
        requirement <'L.btrfs.18'> insertsMerge : ItemLowLevel {
            attribute :>> statement = "RangeSet::insert shall merge a run with the runs
                it touches.";
            attribute :>> criterion = "Runs inserted touching on either side become one
                run.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::insert");
        }
        requirement <'L.btrfs.95'> removesSplit : ItemLowLevel {
            attribute :>> statement = "RangeSet::remove shall split the run a removal
                falls inside.";
            attribute :>> criterion = "A range removed from inside a run leaves the two
                runs around it.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::remove");
        }
        requirement <'L.btrfs.96'> firstFitAligned : ItemLowLevel {
            attribute :>> statement = "RangeSet::first_fit shall return the first start
                that is aligned, holds the length and does not cross the boundary.";
            attribute :>> criterion = "Each fit the test lists is aligned and does not
                cross the boundary.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::first_fit",
                "ferrix_btrfs_write::ranges::fit_in");
        }
        requirement <'L.btrfs.97'> firstPrefixTakesWhatIsThere : ItemLowLevel {
            attribute :>> statement = "RangeSet::first_prefix shall return the longest
                aligned prefix of at most the length wanted and at least the minimum.";
            attribute :>> criterion = "Each request the test lists takes the prefix the
                run has.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::first_prefix");
        }
        requirement <'L.btrfs.98'> addsUnion : ItemLowLevel {
            attribute :>> statement = "RangeSet::add shall union overlapping and
                touching runs.";
            attribute :>> criterion = "Overlapping and touching runs added become their
                union.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::add");
        }
        requirement <'L.btrfs.99'> extractsExact : ItemLowLevel {
            attribute :>> statement = "RangeSet::extract shall take exactly the bytes
                two sets share.";
            attribute :>> criterion = "Extracting one set from another takes exactly
                their intersection and leaves the rest.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::extract");
        }
        requirement <'L.btrfs.19'> everyCopyGetsAStripe : ItemLowLevel {
            attribute :>> statement = "place_stripes shall place every copy of a new
                chunk in the device's holes, or answer None.";
            attribute :>> criterion = "In one 8 MiB hole two copies of 8 MiB are refused
                and two of 4 MiB are placed side by side.";
            attribute :>> parent = "H.STORE.5";
            attribute :>> unit = ("ferrix_btrfs_write::grow::place_stripes",
                "ferrix_btrfs_write::grow::WriteVolume::place_chunk",
                "ferrix_btrfs_write::grow::WriteVolume::device_holes");
        }
        requirement <'L.btrfs.100'> aFullDeviceRefusesData : ItemLowLevel {
            attribute :>> statement = "WriteVolume::alloc_data shall answer NoSpace,
                aborting nothing, when neither a block group nor the device has room for
                a data extent.";
            attribute :>> criterion = "Data extents taken until the device has no room
                end in NoSpace with the transaction not aborted.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::alloc_data",
                "ferrix_btrfs_write::volume::WriteVolume::grow",
                "ferrix_btrfs_write::grow::WriteVolume::place_chunk");
        }
    }

    package Full {
        doc /* A volume that runs out, and a transaction that fails. */

        requirement <'L.btrfs.20'> aWritePastTheRoomChangesNothing : ItemLowLevel {
            attribute :>> statement = "A file write larger than the data room shall be
                refused with NoSpace before it changes anything, aborting nothing.";
            attribute :>> criterion = "A write one sector past data_room is NoSpace, the
                transaction is not aborted, and its commit leaves a volume that checks
                clean.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::admit",
                "ferrix_btrfs_write::volume::WriteVolume::alloc_data",
                "ferrix_btrfs_write::grow::WriteVolume::data_room");
        }
        requirement <'L.btrfs.101'> theWholeRoomFits : ItemLowLevel {
            attribute :>> statement = "A file write of exactly the data room shall
                succeed and leave no data room.";
            attribute :>> criterion = "A write of exactly data_room succeeds, data_room
                is then 0, and the commit checks clean.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::alloc_data",
                "ferrix_btrfs_write::grow::WriteVolume::data_room",
                "ferrix_btrfs_write::grow::WriteVolume::unallocated");
        }
        requirement <'L.btrfs.102'> fullTreesRefuseCreates : ItemLowLevel {
            attribute :>> statement = "An operation the metadata room cannot hold, with
                the commit's reserve and the global reserve, shall be refused with
                NoSpace before its first edit, and an operation that frees space may
                still use the global reserve.";
            attribute :>> criterion = "Creates until the trees are full end in NoSpace
                with the transaction not aborted, the commit succeeds, 64 unlinks
                succeed, and a create after their commit succeeds.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::admit",
                "ferrix_btrfs_write::volume::WriteVolume::commit_reserve",
                "ferrix_btrfs_write::volume::WriteVolume::guarded_with",
                "ferrix_btrfs_write::volume::WriteVolume::operation_with",
                "ferrix_btrfs_write::grow::WriteVolume::meta_room",
                "ferrix_btrfs_write::grow::WriteVolume::tree_headroom");
        }
        requirement <'L.btrfs.103'> fullTreesRefuseData : ItemLowLevel {
            attribute :>> statement = "A data write on full trees shall be refused with
                NoSpace, aborting nothing.";
            attribute :>> criterion = "After the trees are filled, 64 KiB writes end in
                NoSpace with the transaction not aborted, and the commit checks clean.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::admit",
                "ferrix_btrfs_write::volume::WriteVolume::operation_with");
        }
        requirement <'L.btrfs.104'> aChunkMadeMidWriteAbortsNothing : ItemLowLevel {
            attribute :>> statement = "A data write that needs a new metadata chunk
                shall succeed or be refused with NoSpace, aborting nothing.";
            attribute :>> criterion = "With metadata used until the next edit must make
                a chunk, a write of the data room succeeds or is NoSpace, the
                transaction is not aborted, and the commit checks clean.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit =
                ("ferrix_btrfs_write::volume::WriteVolume::ensure_space",
                "ferrix_btrfs_write::volume::WriteVolume::guarded_with",
                "ferrix_btrfs_write::volume::WriteVolume::grow");
        }
        requirement <'L.btrfs.105'> aFullMountAnswersEnospc : ItemLowLevel {
            attribute :>> statement = "Through a mount, a write to a full volume shall
                be refused with ENOSPC at exactly the room statfs offered, keeping what
                was written.";
            attribute :>> criterion = "Through ferrix-btrfs-vfs, writes stop with ENOSPC
                at the byte statfs offered, the file holds every byte written, and after
                an unlink a 1 MiB file is written and read back after remounting.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::admit",
                "ferrix_btrfs_write::grow::WriteVolume::data_room");
        }
        requirement <'L.btrfs.106'> aMountOnFullTreesStaysWritable : ItemLowLevel {
            attribute :>> statement = "Through a mount, creates on full trees shall be
                refused with ENOSPC, and the mount shall stay writable.";
            attribute :>> criterion = "Through ferrix-btrfs-vfs, creates end in ENOSPC,
                another create is ENOSPC too, the sync succeeds and 64 unlinks
                succeed.";
            attribute :>> parent = "H.STORE.6";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::admit",
                "ferrix_btrfs_write::volume::WriteVolume::operation_with");
        }
        requirement <'L.btrfs.21'> anAbortedTransactionIsDropped : ItemLowLevel {
            attribute :>> statement = "A failure after an edit changed something shall
                mark the transaction aborted, and reload_read_only shall replace it with
                the last commit read from the device and answer ReadOnly to every edit
                after it.";
            attribute :>> criterion = "Through a mount, a commit whose device write
                fails answers EIO; the mount then reads the last commit, has no trace of
                the aborted transaction, answers EROFS to a write, a truncation and a
                create, and the disk holds the last commit.";
            attribute :>> parent = "H.STORE.8";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::abort_with",
                "ferrix_btrfs_write::volume::WriteVolume::check_writable",
                "ferrix_btrfs_write::volume::WriteVolume::guarded_with",
                "ferrix_btrfs_write::volume::WriteVolume::operation_with",
                "ferrix_btrfs_write::open::WriteVolume::reload_read_only",
                "ferrix_btrfs_write::device::device_error",
                "ferrix_btrfs_write::device::write");
        }
        requirement <'L.btrfs.107'> anUnreadableLastCommitStaysAborted : ItemLowLevel {
            attribute :>> statement = "reload_read_only shall leave the volume aborted,
                and every read answering an error, when the last commit cannot be
                read.";
            attribute :>> criterion = "Through a mount, with writes and reads failing,
                the sync answers EIO and every later lookup answers EIO.";
            attribute :>> parent = "H.STORE.8";
            attribute :>> unit =
                ("ferrix_btrfs_write::open::WriteVolume::reload_read_only",
                "ferrix_btrfs_write::open::WriteVolume::abort");
        }
        requirement <'L.btrfs.118'> theWriterMapsEachDeviceFailure : ItemLowLevel {
            attribute :>> statement = "The write path shall answer a device failure for
                want of memory with OutOfMemory, as it is, from any operation; a failed
                read with DeviceRead at the physical address it asked for; a failed
                write with DeviceWrite at the address it asked for, or with Flush when
                it was the flush of a durable write that failed; and a failed flush
                with Flush; whatever address the device reported.";
            attribute :>> criterion = "Each of ferrix-fs-device's four errors, each
                reporting an address other than the one asked, from a read, a write and
                a flush, maps through device_error to exactly that table, and a device
                failing every call with each error answers it through the write path's
                read, write, write_durable and flush.";
            attribute :>> parent = "H.STORE.8";
            attribute :>> unit = ("ferrix_btrfs_write::device::device_error",
                "ferrix_btrfs_write::device::read",
                "ferrix_btrfs_write::device::write",
                "ferrix_btrfs_write::device::write_durable",
                "ferrix_btrfs_write::device::flush");
        }
        requirement <'L.btrfs.22'> allocationsAreFallible : ItemLowLevel {
            attribute :>> statement = "Every heap allocation of the product code of
                ferrix-btrfs, ferrix-btrfs-write and ferrix-fs-device shall be made
                through a helper that reports a refused allocation as an error, never
                through one that stops the element.";
            attribute :>> criterion = "tools/common/check/check-fallible-alloc.py finds
                0 unmarked allocating calls in ferrix-btrfs, in ferrix-btrfs-write and
                in ferrix-fs-device.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::fallible::zeroed",
                "ferrix_btrfs_write::fallible::filled",
                "ferrix_btrfs_write::fallible::copy",
                "ferrix_btrfs_write::fallible::with_capacity",
                "ferrix_btrfs_write::fallible::push",
                "ferrix_btrfs_write::fallible::insert_at",
                "ferrix_btrfs_write::fallible::extend_from_slice",
                "ferrix_btrfs_write::fallible::resize",
                "ferrix_btrfs_write::fallible::collect",
                "ferrix_btrfs_write::fallible::collect_ok",
                "ferrix_btrfs_write::fallible::split_off",
                "ferrix_btrfs_write::fallible::copy_map");
        }
        requirement <'L.btrfs.108'> anOperationOutOfMemoryAbortsOrChangesNothing : ItemLowLevel {
            attribute :>> statement = "An operation of the write path whose allocation
                is refused shall answer OutOfMemory, and either leave the transaction
                as it was or abort it, so that the volume reopens at its last commit.";
            attribute :>> criterion = "With each allocation of a script of eleven file
                operations and a commit on a populated volume failed once in turn,
                every failing operation answers OutOfMemory; where the transaction was
                not aborted, the script resumed from that operation ends with the fs
                tree an undisturbed run makes; where it was, the volume reopens at the
                commit before the script with its fs tree; every volume checks clean.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::guarded_with",
                "ferrix_btrfs_write::volume::WriteVolume::operation_with",
                "ferrix_btrfs_write::volume::WriteVolume::abort_with",
                "ferrix_btrfs_write::open::WriteVolume::abort");
        }
        requirement <'L.btrfs.109'> anOpenOutOfMemoryWritesNothing : ItemLowLevel {
            attribute :>> statement = "WriteVolume::open shall answer OutOfMemory when an
                allocation is refused, having written nothing to the device.";
            attribute :>> criterion = "With each allocation of opening the blank and the
                populated volume failed in turn, once and from then on, each open
                succeeds or answers OutOfMemory, and the device then opens at the same
                generation with the same fs tree and checks clean.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::open::WriteVolume::open",
                "ferrix_btrfs_write::open::WriteVolume::open_committed",
                "ferrix_btrfs_write::open::WriteVolume::load_block_groups",
                "ferrix_btrfs_write::open::WriteVolume::load_free_space");
        }
        requirement <'L.btrfs.110'> theReaderPassesOutOfMemoryOn : ItemLowLevel {
            attribute :>> statement = "The reader shall answer a device read that ran out
                of memory with OutOfMemory, reading no other copy and reporting no
                damage.";
            attribute :>> criterion = "With each device read of an open, a lookup and a
                whole-file read failed in turn with OutOfMemory, on a plain, a zstd and
                a DUP image, the answer is OutOfMemory and no read follows the failed
                one.";
            attribute :>> parent = "H.STORE.2";
            attribute :>> unit = ("ferrix_btrfs::volume::Volume::read_node",
                "ferrix_btrfs::fs::read_checked",
                "ferrix_btrfs::volume::read_error",
                "ferrix_btrfs::volume::read_device");
        }
        requirement <'L.btrfs.111'> theWriterPassesOutOfMemoryOn : ItemLowLevel {
            attribute :>> statement = "WriteVolume::read_node shall answer a device read
                that ran out of memory with OutOfMemory, reading no other copy.";
            attribute :>> criterion = "With the first copy of a DUP fs tree root failing
                with OutOfMemory, a lookup answers OutOfMemory after that one read,
                aborts nothing, and succeeds once the device reads again.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::volume::WriteVolume::read_node",
                "ferrix_btrfs::volume::read_device",
                "ferrix_btrfs::volume::read_error");
        }
        requirement <'L.btrfs.112'> aRangeSetEditOutOfMemoryChangesNothing : ItemLowLevel {
            attribute :>> statement = "A RangeSet insert, removal or union that has to make
                a run of its own shall, when that allocation is refused, answer
                OutOfMemory with the set unchanged; one that edits a run in place shall
                allocate nothing.";
            attribute :>> criterion = "Six edits that make a run, each with its first
                allocation failed, answer OutOfMemory and leave the set equal to what it
                was; three in-place edits under the same failure succeed.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::ranges::RangeSet::try_insert",
                "ferrix_btrfs_write::ranges::RangeSet::try_remove",
                "ferrix_btrfs_write::ranges::RangeSet::try_add");
        }
        requirement <'L.btrfs.113'> nothingAllocatesPastTheCommitPoint : ItemLowLevel {
            attribute :>> statement = "A commit shall make everything it needs after
                writing the primary superblock before writing it, so that running out of
                memory is never reported for a commit that is on the device.";
            attribute :>> criterion = "With each allocation of a script of file
                operations and a commit on the blank volume failed from then on, in
                turn, no run whose failure was reported finds the script's commit on the
                device when the volume is reopened.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::commit::WriteVolume::commit",
                "ferrix_btrfs_write::commit::WriteVolume::write_superblocks",
                "ferrix_btrfs_write::commit::WriteVolume::finish_commit",
                "ferrix_btrfs_write::space::Space::prepare_unpin",
                "ferrix_btrfs_write::space::Space::unpin");
        }
        requirement <'L.btrfs.114'> aReplayOutOfMemoryReplaysLater : ItemLowLevel {
            attribute :>> statement = "An open whose log replay is refused an allocation
                shall leave the device so that a later open replays the log.";
            attribute :>> criterion = "With each allocation of opening a volume that
                holds a committed log failed in turn, the open answers OutOfMemory or
                succeeds, and the next open of the same device holds the logged file's
                bytes, commits and checks clean.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = "ferrix_btrfs_write::log::WriteVolume::replay_log";
        }
        requirement <'L.btrfs.115'> aReadOutOfMemoryChangesNothing : ItemLowLevel {
            attribute :>> statement = "A read of the write path whose allocation is
                refused shall answer OutOfMemory and abort nothing.";
            attribute :>> criterion = "With each allocation of reading a populated
                volume's whole fs tree from cold failed in turn, the read answers
                OutOfMemory, the transaction is not aborted, and the tree then reads
                whole.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::tree::WriteVolume::range",
                "ferrix_btrfs_write::tree::WriteVolume::next_item",
                "ferrix_btrfs_write::volume::WriteVolume::load");
        }
        requirement <'L.btrfs.116'> mapNodesFitASizeClass : ItemLowLevel {
            attribute :>> statement = "Every ordered map the write path keeps shall have
                nodes no larger than the kernel heap's largest size class, so that the
                kernel's reserve can serve its inserts.";
            attribute :>> criterion = "The node bound of each of the ten map and set
                types the write path keeps is at most ferrix_heap::LARGEST_CLASS.";
            attribute :>> parent = "H.STORE.7";
            attribute :>> unit = ("ferrix_btrfs_write::fallible::insert",
                "ferrix_btrfs_write::fallible::entry",
                "ferrix_btrfs_write::fallible::insert_into_set");
        }
    }
}
