/*
 * Ferrix — SysML v2 model, part 26: the requirements of product signing's
 * verifier.
 *
 * docs/SIGNING.md is the design (the certification consultant's OK IF at
 * ledger lines 3667 and 3738; the customer's decisions at 3731). Slice S1
 * builds the verify-only crate `ferrix-sigverify` (src/lib/crypto/sigverify)
 * and the hash it rests on, `ferrix-sha256` (src/lib/crypto/sha256), both
 * in the item ring (certification-item.json's crates). What the UEFI
 * loader, init's configuration verifier and devmgr do with the verifier's
 * answer is theirs, and gets ids in their own slices (S4, S8, S9).
 *
 * The parent. H.SIGN is parented by ASR-7, "data supplied by a partition
 * shall be validated before use", alone (ledger 3738 (7)). Slice S4 adds
 * the update objective, its threat, FCS_COP.1/SigVer and FPT_TUD_EXT.1 to
 * the Security Target, and re-parents H.SIGN then.
 *
 * What proves what. Host tests in each crate's src/tests.rs, against the
 * published vectors (RFC 9858 Appendix A, RFC 8554 Appendix F, NIST ACVP
 * LMS sigVer, NIST CAVP SHA-256) and against manifests the development soft
 * signer (tools/common/release/softsign) makes; each comparison's negative
 * control is run by fleet/gate.sh control. The time and stack bounds wait
 * for slice S2's measurement on the DK1 and stay in the baseline until then.
 *
 * Ids are `H.SIGN.<n>`, `L.sigverify.<n>` and `L.sha256.<n>`, reserved on
 * develop at 81081184a. They are never renumbered once a check names them.
 */
package FerrixSigningRequirements {
    private import FerrixItemRequirements::*;

    package Signing {
        doc /* The verifier's own decisions: what it accepts, and why it
             * refuses everything else. */

        requirement <'H.SIGN.1'> acceptedOnlyUnderTheExpectedKindsAnchor : ItemHighLevel {
            attribute :>> statement = "The verifier shall accept a manifest only
                when its HSS signature over the manifest's first 512 bytes
                verifies, under the allowed parameter sets, against an anchor of
                the kind the caller expects with the anchor id the manifest
                signs.";
            attribute :>> criterion = "RFC 9858 A.4 verifies; every bit flip of it
                is refused; another kind's anchor, an unknown id or another key is
                refused; ACVP's sample cases give their answers.";
            attribute :>> parent = "ASR-7";
        }
        requirement <'H.SIGN.2'> signedFieldsBind : ItemHighLevel {
            attribute :>> statement = "The verifier shall accept a manifest only
                when its signed kind, profile, arch, hardware family and subject
                are the ones the caller expects, shall refuse a configuration or
                driver manifest whose caller names no subject, and shall confirm a
                payload only when its length and SHA-256 are the ones the manifest
                signs.";
            attribute :>> criterion = "Each bound field one off is refused by its
                name; a payload one byte short, long or different is refused.";
            attribute :>> parent = "ASR-7";
        }
        requirement <'H.SIGN.3'> notBelowTheFloor : ItemHighLevel {
            attribute :>> statement = "The verifier shall refuse a manifest whose
                security version is below the floor the caller holds for its
                kind.";
            attribute :>> criterion = "A manifest one below the floor is refused,
                one at the floor accepted.";
            attribute :>> parent = "ASR-7";
        }
        requirement <'H.SIGN.4'> revokedKeysRefused : ItemHighLevel {
            attribute :>> statement = "The verifier shall refuse a manifest signed
                through a tree the caller holds as revoked, and a revocation that
                lists its own signing key.";
            attribute :>> criterion = "A revoked key's manifest, a revocation by a
                revoked key, and a revocation of its own key are each refused.";
            attribute :>> parent = "ASR-7";
        }
        requirement <'H.SIGN.5'> everythingElseRefusedByName : ItemHighLevel {
            attribute :>> statement = "The verifier shall answer every input with
                either a verified manifest or one named refusal, with no path that
                accepts by default and no panic.";
            attribute :>> criterion = "Every byte of a good block changed is
                refused; every structural field is refused by its name; every
                truncation answers; the fuzz target finds no panic.";
            attribute :>> parent = "ASR-7";
        }
        requirement <'H.SIGN.6'> boundedTimeAndMemory : ItemHighLevel {
            attribute :>> statement = "One verification shall allocate nothing and
                finish within the bound docs/certification/MEMORY-AND-TIMING.md
                states for the slowest target, the DK1's Cortex-A7 at 650 MHz.";
            attribute :>> criterion = "Slice S2's measurement of the worst-case
                signature on the DK1 within the bound, and the stack report.";
            attribute :>> parent = "ASR-7";
        }
        requirement <'H.SIGN.7'> loaderStartsOnlyWhatThePolicyAllows : ItemHighLevel {
            attribute :>> statement = "The UEFI loader shall start a slot only when
                ferrix-boot-policy returned Start for it and every blob matched its
                signed digest on the copy it starts.";
            attribute :>> criterion = "Host: decide's tests; boot: test-ab-boot-signed's
                S1-S3, S10, N1-N3 and R1, each with a control.";
            attribute :>> parent = "ASR-7";
        }
    }

    package SigVerify {
        doc /* ferrix-sigverify: the manifest and LMS/HSS verification. */

        requirement <'L.sigverify.1'> manifestStructure : ItemLowLevel {
            attribute :>> statement = "structure shall refuse, naming the field, a
                block that is not 4,096 bytes, a wrong magic, format or body
                length, an unknown kind, non-zero flags, an unknown profile or
                arch, non-zero reserved bytes, a slot's non-zero subject, floors
                outside a slot manifest, revocation fields outside a revocation, a
                revocation count past 20 or an entry past its count, non-zero
                padding, a signature length past 3,532 or the block, and a
                non-zero byte after the signature.";
            attribute :>> criterion = "Host tests: each field refused by its name;
                every byte of a good block changed is refused; sig_len one short
                and one long is refused.";
            attribute :>> parent = ("H.SIGN.5", "H.SIGN.1");
            attribute :>> unit = ("ferrix_sigverify::manifest::structure",
                "ferrix_sigverify::manifest::header", "ferrix_sigverify::manifest::kind_fields",
                "ferrix_sigverify::manifest::signature_of",
                "ferrix_sigverify::manifest::entry_offset",
                "ferrix_sigverify::manifest::bytes",
                "ferrix_sigverify::manifest::Kind::from_code");
        }
        requirement <'L.sigverify.2'> onlyTheProductTypeCodes : ItemLowLevel {
            attribute :>> statement = "A key or signature shall be accepted by the
                public entry points only with LM-OTS type LMOTS_SHA256_N24_W4 and
                LMS types LMS_SHA256_M24_H10, _H15 or _H20; every other type code
                shall be refused as a type code.";
            attribute :>> criterion = "Host tests: every code from 0 to 0x18 and
                0xFFFFFFFF, of both types; RFC 9858 A.1 (W8, H5) verifies only in
                the wide test set; RFC 8554 Test Case 1 is refused.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = ("ferrix_sigverify::lms::Allowed::ots",
                "ferrix_sigverify::lms::Allowed::lms", "ferrix_sigverify::lms::LmsKey::parse_in");
        }
        requirement <'L.sigverify.3'> lmsVerifiedAsRfc8554 : ItemLowLevel {
            attribute :>> statement = "lms_verify_in shall accept an LMS signature
                only when its length is exactly its types', its leaf index is
                below 2^h, its LM-OTS type is the key's, and the root computed by
                RFC 8554 Algorithms 4b and 6a over SHA-256/192 equals the key's.";
            attribute :>> criterion = "Host tests: RFC 9858 A.4 verifies and every
                single-bit flip of its signature and message is refused; a leaf
                index of 2^20 is refused as a leaf; every NIST ACVP LMS sigVer
                sample case gives ACVP's answer.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = ("ferrix_sigverify::lms::ots_candidate",
                "ferrix_sigverify::lms::with_checksum", "ferrix_sigverify::lms::root_from",
                "ferrix_sigverify::lms::coef", "ferrix_sigverify::lms::chain_step",
                "ferrix_sigverify::lms::lms_verify_in",
                "ferrix_sigverify::lms::signature_len_for",
                "ferrix_sigverify::lms::Reader::take", "ferrix_sigverify::lms::Reader::array",
                "ferrix_sigverify::lms::Reader::u32");
        }
        requirement <'L.sigverify.4'> hssVerifiedAsRfc8554 : ItemLowLevel {
            attribute :>> statement = "hss_verify_in shall accept an HSS signature
                only when the key's L is 1 or 2, Nspk + 1 is L, each signed lower
                key verifies under the key above it, the last LMS signature
                verifies over the message, and no byte is short or left over; it
                shall return the trees it went through, top first.";
            attribute :>> criterion = "Host tests: A.4 verifies with its I; L of 0,
                3 and 8 and Nspk one off are refused; A.4 cut or extended by a byte
                and with each key byte changed is refused.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = ("ferrix_sigverify::lms::hss_verify_in",
                "ferrix_sigverify::lms::lower_key", "ferrix_sigverify::lms::parse_hss_key_in");
        }
        requirement <'L.sigverify.5'> anchorOfTheExpectedKind : ItemLowLevel {
            attribute :>> statement = "verify shall take the anchor from those of
                the caller's expected kind (a revocation's: its target kind's)
                with the manifest's signed anchor id, and refuse a manifest with no
                such anchor.";
            attribute :>> criterion = "Host tests: a slot manifest checked against
                only a configuration anchor, against a renumbered anchor, and
                against the right id with another key, each refused.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = ("ferrix_sigverify::manifest::verify",
                "ferrix_sigverify::manifest::anchor_for");
        }
        requirement <'L.sigverify.6'> signedFieldsBound : ItemLowLevel {
            attribute :>> statement = "verify shall refuse a manifest whose kind,
                profile, arch, family or subject is not what the caller expects,
                the kind before the signature is checked and the rest after, and a
                configuration or driver manifest whose caller names no subject.";
            attribute :>> criterion = "Host tests: each bound field one off refused
                by its name; a driver manifest binds its subject; a slot manifest
                presented as a revocation, and a revocation presented as a slot,
                refused by the signed kind.";
            attribute :>> parent = "H.SIGN.2";
            attribute :>> unit = ("ferrix_sigverify::manifest::verify",
                "ferrix_sigverify::manifest::bindings");
        }
        requirement <'L.sigverify.7'> floorHeld : ItemLowLevel {
            attribute :>> statement = "verify shall refuse a manifest whose
                security version is below the caller's floor.";
            attribute :>> criterion = "Host tests: a manifest one below the floor
                refused; one at the floor, and one above a zero floor, accepted.";
            attribute :>> parent = "H.SIGN.3";
            attribute :>> unit = "ferrix_sigverify::manifest::verify";
        }
        requirement <'L.sigverify.8'> revokedTreesRefused : ItemLowLevel {
            attribute :>> statement = "verify shall refuse a manifest signed through
                a tree the caller holds as revoked, a revocation among them.";
            attribute :>> criterion = "Host tests: a revoked key's manifest and a
                revocation by a revoked key are refused; another key's revocation
                leaves a manifest alone.";
            attribute :>> parent = "H.SIGN.4";
            attribute :>> unit = ("ferrix_sigverify::manifest::verify",
                "ferrix_sigverify::manifest::revocations");
        }
        requirement <'L.sigverify.13'> selfRevocationRefused : ItemLowLevel {
            attribute :>> statement = "verify shall refuse a revocation that lists a
                tree its own signature went through.";
            attribute :>> criterion = "Host test: a revocation of its own key is
                refused as revoking itself.";
            attribute :>> parent = "H.SIGN.4";
            attribute :>> unit = ("ferrix_sigverify::manifest::revocations",
                "ferrix_sigverify::manifest::revocation_of");
        }
        requirement <'L.sigverify.14'> revokedListReturned : ItemLowLevel {
            attribute :>> statement = "revoked shall return a verified revocation's
                target kind and exactly the trees it lists, and nothing for any
                other kind.";
            attribute :>> criterion = "Host tests: a verified revocation returns its
                one tree and its target; a verified slot manifest returns
                nothing.";
            attribute :>> parent = "H.SIGN.4";
            attribute :>> unit = ("ferrix_sigverify::manifest::Verified::revoked",
                "ferrix_sigverify::manifest::revocation_of");
        }
        requirement <'L.sigverify.9'> payloadConfirmed : ItemLowLevel {
            attribute :>> statement = "check_payload shall confirm a payload only
                when its length and SHA-256 equal the verified manifest's.";
            attribute :>> criterion = "Host tests: the manifest's own length and
                digest confirmed; one byte short, one long, and another digest
                refused.";
            attribute :>> parent = "H.SIGN.2";
            attribute :>> unit = "ferrix_sigverify::manifest::Verified::check_payload";
        }
        requirement <'L.sigverify.10'> refusalsTotal : ItemLowLevel {
            attribute :>> statement = "Every failure of LMS/HSS verification shall
                reach the caller as one of the manifest's named refusals, and no
                input shall reach a panic.";
            attribute :>> criterion = "Host tests: every truncation of a good block
                and of A.4's signature answers; the sigverify_manifest fuzz target
                runs bounded with no crash.";
            attribute :>> parent = "H.SIGN.5";
            attribute :>> unit = "ferrix_sigverify::manifest::Refusal::from";
        }
        requirement <'L.sigverify.11'> verifyTimeBound : ItemLowLevel {
            attribute :>> statement = "One hss_verify of a product-set signature
                shall finish within 50 ms on the DK1's Cortex-A7 at 650 MHz.";
            attribute :>> criterion = "Slice S2: the worst-case signature measured
                on the DK1 (the estimate is SIGNING.md section 2.3's).";
            attribute :>> parent = "H.SIGN.6";
            attribute :>> unit = "ferrix_sigverify::lms::hss_verify_in";
        }
        requirement <'L.sigverify.12'> verifyStackBound : ItemLowLevel {
            attribute :>> statement = "One verify shall allocate nothing and use at
                most the stack MEMORY-AND-TIMING.md states.";
            attribute :>> criterion = "The crate has no allocator and no alloc
                crate; the stack report of the loader's build (S4).";
            attribute :>> parent = "H.SIGN.6";
            attribute :>> unit = "ferrix_sigverify::manifest::verify";
        }
    }

    package Sha256 {
        doc /* ferrix-sha256: the one hash under the verifier. */

        requirement <'L.sha256.1'> sha256AsFips180 : ItemLowLevel {
            attribute :>> statement = "digest shall return the SHA-256 of FIPS 180-4
                for every message.";
            attribute :>> criterion = "Host tests: FIPS 180-4's examples and the
                NIST CAVP SHA256ShortMsg, SHA256LongMsg and SHA256Monte files.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = ("ferrix_sha256::digest", "ferrix_sha256::Sha256::finish",
                "ferrix_sha256::compress", "ferrix_sha256::rounds_0_to_31",
                "ferrix_sha256::rounds_32_to_63");
        }
        requirement <'L.sha256.2'> sha256InAnyPieces : ItemLowLevel {
            attribute :>> statement = "A message given to update in any pieces shall
                hash as the whole message does.";
            attribute :>> criterion = "Host tests: every length from 0 to 130 bytes
                byte by byte, and a million bytes in pieces of 997.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = "ferrix_sha256::Sha256::update";
        }
        requirement <'L.sha256.3'> sha256Truncated192 : ItemLowLevel {
            attribute :>> statement = "finish_192 and digest_192 shall return the
                first 24 bytes of the SHA-256 digest (RFC 9858 section 2.1).";
            attribute :>> criterion = "Host test: RFC 9858's SHA-256/192 of abc,
                whole and in pieces.";
            attribute :>> parent = "H.SIGN.1";
            attribute :>> unit = ("ferrix_sha256::finish_192", "ferrix_sha256::digest_192");
        }
    }

    package BootPolicy {
        doc /* ferrix-boot-policy: whether the UEFI loader may start a slot
             * (docs/SIGNING.md section 10.6; ledger 4016, 4032). */

        requirement <'L.bootpolicy.1'> policyBlockFailsClosed : ItemLowLevel {
            attribute :>> statement = "parse_block shall refuse every slot, by its own refusal, for a trust-policy block shorter than 512 bytes, of another magic or version, of an unknown mode, with more than eight anchors, with an anchor of a kind other than the slot kind, or with a non-zero reserved byte, unused entry or tail; encode_block shall lay a block out as parse_block reads it.";
            attribute :>> criterion = "Host tests: one per case, each with a control (a_short_block_refuses_every_slot, an_unknown_block_version_or_mode_refuses_every_slot, a_count_above_eight_refuses_every_slot, non_zero_block_padding_refuses_every_slot, a_non_slot_anchor_kind_refuses_every_slot); the four generated blocks parse as named.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_boot_policy::parse_block",
                "ferrix_boot_policy::anchor_entries",
                "ferrix_boot_policy::encode_block",
                "ferrix_boot_policy::put");
        }
        requirement <'L.bootpolicy.2'> version1OnlyUnderOff : ItemLowLevel {
            attribute :>> statement = "decide shall start a version-1 slot on its hashes, unsigned, under mode off, and refuse it as Unsigned under mode required.";
            attribute :>> criterion = "Host test: both modes.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.3'> version2GenerationZero : ItemLowLevel {
            attribute :>> statement = "decide shall refuse a version-2 header whose generation is not zero.";
            attribute :>> criterion = "Host test: generation 9 refused as Header(Generation).";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.4'> version2VerifiedAgainstTheBlock : ItemLowLevel {
            attribute :>> statement = "decide shall verify a version-2 slot's manifest with ferrix-sigverify against the block's anchors with the slot kind expected, and return any refusal.";
            attribute :>> criterion = "Host tests: a good slot starts under both modes; another key, an empty table, another key as the anchor and a configuration manifest are refused.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.5'> version2NeverUnverified : ItemLowLevel {
            attribute :>> statement = "decide shall never start a version-2 slot unverified under either mode.";
            attribute :>> criterion = "Host tests: a missing, short or zero manifest block, and every fifth byte of the signed body changed, under off.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.6'> manifestCoversThisHeader : ItemLowLevel {
            attribute :>> statement = "decide shall start a version-2 slot only when its manifest's payload is the header's seal over its 4064 sealed bytes.";
            attribute :>> criterion = "Host tests: a good manifest on another header, and the right digest with length 4096, are refused as PayloadMismatch.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.7'> profileAndArchBound : ItemLowLevel {
            attribute :>> statement = "A manifest whose profile or arch differs from the build's shall be refused, by its own refusal.";
            attribute :>> criterion = "Host tests: another profile WrongProfile, another arch WrongArch.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.8'> familiesFollowTheClaim : ItemLowLevel {
            attribute :>> statement = "The families decide accepts shall be the build's families that are the machine's own and, without a claim, its arch's generic one; with a claim, exactly the machine's own.";
            attribute :>> criterion = "Host tests: an N150 manifest with and without a claim on an N150 and on a generic machine; an empty family list.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_boot_policy::accepted",
                "ferrix_boot_policy::generic_family");
        }
        requirement <'L.bootpolicy.9'> floorIsTheCallers : ItemLowLevel {
            attribute :>> statement = "decide shall refuse a manifest whose security version is below the floor its caller passes.";
            attribute :>> criterion = "Host test: floors 0 and 3 start, 4 is refused as BelowFloor.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::decide";
        }
        requirement <'L.bootpolicy.10'> rescueFollowsTheMode : ItemLowLevel {
            attribute :>> statement = "rescue shall offer the ESP's unsigned files only under mode off, and only the rescue slot image under mode required.";
            attribute :>> criterion = "Host test: both modes; a malformed block offers nothing.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_boot_policy::rescue";
        }
        requirement <'L.bootpolicy.11'> startNamesTheSignature : ItemLowLevel {
            attribute :>> statement = "Start shall carry the header to match and the verified anchor id and security version.";
            attribute :>> criterion = "Host test: the anchor and version of a good slot.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_boot_policy::decide",
                "ferrix_boot_policy::Policy::anchors");
        }
        requirement <'L.bootpolicy.12'> everyInputAnswers : ItemLowLevel {
            attribute :>> statement = "decide shall answer every input with Start or one Refusal, and never panic.";
            attribute :>> criterion = "Host test: 300 randomly damaged slot heads, whole and truncated; the panic audit.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_boot_policy::decide",
                "ferrix_boot_policy::byte",
                "ferrix_boot_policy::le16");
        }
    }

    package Slots {
        doc /* ferrix-slots, in the item since slice S4 (docs/AB-BOOT.md
             * sections 2-4; docs/SIGNING.md section 5.3). */

        requirement <'L.slots.1'> crcCheckValues : ItemLowLevel {
            attribute :>> statement = "crc32 and crc32c shall give the standard check values.";
            attribute :>> criterion = "Host test: both over 123456789.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_slots::crc::reflected";
        }
        requirement <'L.slots.2'> recordRoundTripsAndRefuses : ItemLowLevel {
            attribute :>> statement = "Record::decode shall return what Record::encode wrote, and refuse a copy whose magic, version, length, CRC, reserved bytes or fields are wrong or inconsistent.";
            attribute :>> criterion = "Host tests: a round trip; every bad field refused.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::record::Record::encode",
                "ferrix_slots::record::Record::decode",
                "ferrix_slots::record::Record::is_consistent");
        }
        requirement <'L.slots.3'> newerValidCopyIsCurrent : ItemLowLevel {
            attribute :>> statement = "select shall take the valid copy with the higher sequence, copy 0 on a tie, and write_copy the copy that does not hold it, so that a torn write leaves the old record or the new.";
            attribute :>> criterion = "Host tests: every pair of valid, torn and zero copies; a write torn at every byte.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::record::select",
                "ferrix_slots::record::write_copy");
        }
        requirement <'L.slots.4'> recordTransitions : ItemLowLevel {
            attribute :>> statement = "after_try, armed, disarmed and marked_good shall make the records AB-BOOT section 3 names, and refuse the transitions it does not.";
            attribute :>> criterion = "Host test: each transition and its refusals.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::record::Record::after_try",
                "ferrix_slots::record::Record::armed",
                "ferrix_slots::record::Record::disarmed",
                "ferrix_slots::record::Record::marked_good");
        }
        requirement <'L.slots.5'> recordWriteCountsOnlyReadBack : ItemLowLevel {
            attribute :>> statement = "write_record shall count a record written only once it is flushed and read back equal, and read_current shall read both copies in whole usable blocks.";
            attribute :>> criterion = "Host test: a device that fails each step.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::record::write_record",
                "ferrix_slots::record::read_current",
                "ferrix_slots::record::usable_block");
        }
        requirement <'L.slots.6'> planFollowsTheRecord : ItemLowLevel {
            attribute :>> statement = "plan shall give AB-BOOT section 4's attempts for every record state.";
            attribute :>> criterion = "Host test: exhaustively over the record's states.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::plan::plan",
                "ferrix_slots::plan::Kind::from_word",
                "ferrix_slots::plan::Kind::word");
        }
        requirement <'L.slots.7'> recordBlocksInside : ItemLowLevel {
            attribute :>> statement = "record_block shall name only blocks inside the control partition, or none.";
            attribute :>> criterion = "Host test: every copy and block size, and one past the end.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_slots::plan::record_block";
        }
        requirement <'L.slots.8'> loaderWordsFirstMatch : ItemLowLevel {
            attribute :>> statement = "The loader's words shall be written whole or not at all, and parse_words shall read them back first-match, a missing ferrix.slot.sig as none and a malformed one as no words.";
            attribute :>> criterion = "Host tests: written, refused for want of room, read back, forged after the loader's.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::plan::LoaderWords::fmt",
                "ferrix_slots::plan::parse_words",
                "ferrix_slots::plan::write_words",
                "ferrix_slots::plan::Cursor::write_str");
        }
        requirement <'L.slots.9'> commandLineWholeOrNot : ItemLowLevel {
            attribute :>> statement = "compose shall write the loader's words then the slot's trimmed line, or nothing when it is not UTF-8 or does not fit.";
            attribute :>> criterion = "Host tests: exact room, one byte short, not UTF-8; the longest line fits the boot info.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_slots::plan::compose";
        }
        requirement <'L.slots.10'> headerRoundTrips : ItemLowLevel {
            attribute :>> statement = "SlotHeader::decode shall return the header SlotHeader::encode wrote for lay_out's blobs, and Blob::matches shall hold only for a blob's own bytes.";
            attribute :>> criterion = "Host test: a slot round trips and verifies; a flipped or short blob does not match.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::slot::SlotHeader::lay_out",
                "ferrix_slots::slot::SlotHeader::encode",
                "ferrix_slots::slot::Blob::matches",
                "ferrix_slots::slot::SlotHeader::label_text");
        }
        requirement <'L.slots.11'> headerRefuses : ItemLowLevel {
            attribute :>> statement = "SlotHeader::decode shall refuse a short page, another magic, version, length or count, a broken seal, non-zero reserved bytes, a misplaced, overlapping or outside blob, no kernel and a command line past 2048 bytes.";
            attribute :>> criterion = "Host test: each, and every seventh bit of the header.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::slot::SlotHeader::decode",
                "ferrix_slots::slot::SlotHeader::check_placement");
        }
        requirement <'L.slots.12'> version2Header : ItemLowLevel {
            attribute :>> statement = "A version-2 header shall hold its blobs at or after 8192 and a zero generation, refused otherwise, and seal shall return the SHA-256 of its bytes 0..4064.";
            attribute :>> criterion = "Host tests: a version-2 layout, its seal, a generation and a blob in the manifest block refused.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_slots::slot::SlotHeader::seal";
        }
        requirement <'L.slots.13'> gptFindsByType : ItemLowLevel {
            attribute :>> statement = "find shall locate exactly one control partition and two slots by type GUID, A the lower entry, and the event-log partition alone.";
            attribute :>> criterion = "Host tests: the layout found by type; the event log found alone and checked.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::gpt::find",
                "ferrix_slots::gpt::find_one",
                "ferrix_slots::gpt::entry_part",
                "ferrix_slots::gpt::Layout::slot",
                "ferrix_slots::gpt::guid");
        }
        requirement <'L.slots.14'> gptTrustsNothing : ItemLowLevel {
            attribute :>> statement = "parse_header and find shall refuse a header or array whose signature, CRC, sizes or counts are wrong, and a Ferrix partition outside the usable blocks or overlapping another entry.";
            attribute :>> criterion = "Host tests: hostile tables; outside the usable blocks; a damaged array.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = "ferrix_slots::gpt::parse_header";
        }
        requirement <'L.slots.15'> guidText : ItemLowLevel {
            attribute :>> statement = "parse_guid and GuidText shall read and write a GUID in its usual text form, either case read.";
            attribute :>> criterion = "Host test: a disk GUID read back in upper and lower case.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::gpt::parse_guid",
                "ferrix_slots::gpt::GuidText::fmt");
        }
        requirement <'L.slots.16'> slotsAndFields : ItemLowLevel {
            attribute :>> statement = "A slot's code and letter shall map both ways, and the little-endian field readers and writer shall read and write the bytes the formats name.";
            attribute :>> criterion = "Host tests: through the record and header round trips.";
            attribute :>> parent = "H.SIGN.7";
            attribute :>> unit = ("ferrix_slots::Slot::code",
                "ferrix_slots::Slot::from_code",
                "ferrix_slots::Slot::other",
                "ferrix_slots::Slot::letter",
                "ferrix_slots::Slot::from_letter",
                "ferrix_slots::le16",
                "ferrix_slots::le32",
                "ferrix_slots::le64",
                "ferrix_slots::put");
        }
    }
}
