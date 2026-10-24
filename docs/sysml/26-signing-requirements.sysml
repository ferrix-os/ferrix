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
                "ferrix_sha256::compress");
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
}
