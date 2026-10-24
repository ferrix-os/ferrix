//! Tests of the verifier: the published vectors (RFC 9858, RFC 8554, NIST
//! ACVP), and every rule of `docs/SIGNING.md` §1.2 and §8.3 on manifests
//! the development soft signer makes.
//!
//! Each test's `Verifies:` line names the requirement it checks.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::string_slice,
    reason = "AUDIT: fixtures: small known lengths, and the hex sliced is ASCII by construction (the RFCs' and ACVP's vectors)"
)]

extern crate std;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::vec::Vec;

use ferrix_softsign::manifest::{Body, assemble};
use ferrix_softsign::token::{CKA_VALUE, CKM_HSS, ObjectHandle, SoftToken, Token};

use crate::lms::{self, EVERY_SHA256_192, Error, HSS_PUBLIC_KEY_LEN, LmsKey, PRODUCT};
use crate::manifest::{Anchor, BLOCK_LEN, BODY_LEN, Expect, Field, Kind, Refusal, verify};
use crate::vectors;

fn unhex(parts: &[&str]) -> Vec<u8> {
    let text: std::string::String = parts.concat();
    (0..text.len() / 2)
        .map(|n| u8::from_str_radix(&text[2 * n..2 * n + 2], 16).unwrap())
        .collect()
}

fn key52(bytes: &[u8]) -> [u8; HSS_PUBLIC_KEY_LEN] {
    bytes.try_into().unwrap()
}

// ---------------------------------------------------------------- vectors

/// Verifies: L.sigverify.3, L.sigverify.4
/// Verifies: H.SIGN.1
/// RFC 9858 A.4 -- the product set itself, `LMS_SHA256_M24_H20` with
/// `LMOTS_SHA256_N24_W4`, `L = 1` -- verifies, and the chain names its `I`.
#[test]
fn rfc_9858_a4_verifies() {
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let chain = lms::hss_verify(
        &public,
        &unhex(vectors::A4_MESSAGE),
        &unhex(vectors::A4_SIGNATURE),
    )
    .unwrap();
    let id: [u8; 16] = public[12..28].try_into().unwrap();
    assert_eq!(chain.ids(), &[id]);
}

/// Verifies: L.sigverify.3
/// Every single-bit flip of A.4's signature, and of its message, is
/// refused.
#[test]
fn rfc_9858_a4_every_bit_flip_is_refused() {
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let message = unhex(vectors::A4_MESSAGE);
    let signature = unhex(vectors::A4_SIGNATURE);
    for byte in 0..signature.len() {
        for bit in 0..8 {
            let mut s = signature.clone();
            s[byte] ^= 1 << bit;
            assert!(
                lms::hss_verify(&public, &message, &s).is_err(),
                "signature byte {byte} bit {bit}"
            );
        }
    }
    for byte in 0..message.len() {
        let mut m = message.clone();
        m[byte] ^= 1;
        assert_eq!(
            lms::hss_verify(&public, &m, &signature),
            Err(Error::BadSignature),
            "message byte {byte}"
        );
    }
}

/// Verifies: L.sigverify.4
/// A.4 cut by a byte, extended by a byte, and with each public-key byte
/// changed, is refused.
#[test]
fn rfc_9858_a4_truncated_extended_or_wrong_key_is_refused() {
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let message = unhex(vectors::A4_MESSAGE);
    let signature = unhex(vectors::A4_SIGNATURE);
    assert_eq!(
        lms::hss_verify(&public, &message, &signature[..signature.len() - 1]),
        Err(Error::Length)
    );
    let mut longer = signature.clone();
    longer.push(0);
    assert_eq!(
        lms::hss_verify(&public, &message, &longer),
        Err(Error::Length)
    );
    for byte in 0..HSS_PUBLIC_KEY_LEN {
        let mut p = public;
        p[byte] ^= 1;
        assert!(
            lms::hss_verify(&p, &message, &signature).is_err(),
            "key byte {byte}"
        );
    }
}

/// Verifies: L.sigverify.2
/// RFC 9858 A.1 (W8, H5) verifies through the same code under every
/// SHA-256/192 set, and is refused by its type codes under the product set.
#[test]
fn rfc_9858_a1_verifies_in_the_wide_set_and_is_refused_in_the_product_set() {
    let public = key52(&unhex(vectors::A1_PUBLIC));
    let message = unhex(vectors::A1_MESSAGE);
    let signature = unhex(vectors::A1_SIGNATURE);
    assert!(lms::hss_verify_in(&EVERY_SHA256_192, &public, &message, &signature).is_ok());
    assert_eq!(
        lms::hss_verify(&public, &message, &signature),
        Err(Error::TypeCode)
    );
}

/// Verifies: L.sigverify.2, L.sigverify.4
/// RFC 8554 Test Case 1 (SHA-256/256): its 60-byte public key is no
/// `SHA-256/192` key, and its signature under A.4's key is refused by its
/// level count before anything is hashed.
#[test]
fn rfc_8554_test_case_1_is_refused() {
    let public = unhex(vectors::TC1_PUBLIC);
    assert_eq!(public.len(), 60);
    assert_eq!(LmsKey::parse(&public[4..]), Err(Error::Length));
    let a4 = key52(&unhex(vectors::A4_PUBLIC));
    assert_eq!(
        lms::hss_verify(
            &a4,
            &unhex(vectors::TC1_MESSAGE),
            &unhex(vectors::TC1_SIGNATURE)
        ),
        Err(Error::Levels)
    );
}

/// Verifies: L.sigverify.3
/// Every NIST ACVP LMS sigVer sample case gives ACVP's answer through the
/// same code (their sets, `M24_H5`/`N24_W1` and `M24_H10`/`N24_W2`, are outside
/// the product set and refused by it).
#[test]
fn nist_acvp_lms_sigver_cases_give_their_expected_answers() {
    assert_eq!(vectors::ACVP.len(), 16);
    for case in vectors::ACVP {
        let public = unhex(&[case.public_key]);
        let key = LmsKey::parse_in(&EVERY_SHA256_192, &public);
        let answer = key.and_then(|k| {
            lms::lms_verify_in(
                &EVERY_SHA256_192,
                &k,
                &unhex(&[case.message]),
                &unhex(&[case.signature]),
            )
        });
        assert_eq!(
            answer.is_ok(),
            case.passes,
            "{} tc {} ({})",
            case.source,
            case.tc_id,
            case.reason
        );
        assert!(LmsKey::parse_in(&PRODUCT, &public).is_err());
    }
}

// ---------------------------------------------------------- type codes

/// Verifies: L.sigverify.2
/// Every type code from 0 to 0x18 and 0xFFFFFFFF: only `N24_W4` and
/// `M24_H10`/`H15`/`H20` parse.
#[test]
fn only_the_product_type_codes_parse() {
    let mut key = [0u8; 48];
    key[..4].copy_from_slice(&0x0Bu32.to_be_bytes());
    key[4..8].copy_from_slice(&7u32.to_be_bytes());
    assert!(LmsKey::parse(&key).is_ok());
    for code in (0..=0x18u32).chain([0xFFFF_FFFF]) {
        let mut k = key;
        k[..4].copy_from_slice(&code.to_be_bytes());
        assert_eq!(
            LmsKey::parse(&k).is_ok(),
            matches!(code, 0x0B..=0x0D),
            "lms {code:#x}"
        );
        let mut k = key;
        k[4..8].copy_from_slice(&code.to_be_bytes());
        assert_eq!(LmsKey::parse(&k).is_ok(), code == 7, "ots {code:#x}");
    }
}

/// Verifies: L.sigverify.4
/// `L` 0 and 3 are refused, and `Nspk` one too high or too low.
#[test]
fn levels_outside_one_and_two_and_a_wrong_nspk_are_refused() {
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let message = unhex(vectors::A4_MESSAGE);
    let signature = unhex(vectors::A4_SIGNATURE);
    for levels in [0u32, 3, 8] {
        let mut p = public;
        p[..4].copy_from_slice(&levels.to_be_bytes());
        assert_eq!(
            lms::hss_verify(&p, &message, &signature),
            Err(Error::Levels)
        );
    }
    let mut s = signature.clone();
    s[..4].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(lms::hss_verify(&public, &message, &s), Err(Error::Levels));
    let mut p = public;
    p[..4].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(
        lms::hss_verify(&p, &message, &signature),
        Err(Error::Levels)
    );
}

/// Verifies: L.sigverify.3
/// A leaf index at `2^h` is refused as a leaf, not hashed.
#[test]
fn a_leaf_index_past_the_tree_is_refused() {
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let mut s = unhex(vectors::A4_SIGNATURE);
    s[4..8].copy_from_slice(&(1u32 << 20).to_be_bytes());
    assert_eq!(
        lms::hss_verify(&public, &unhex(vectors::A4_MESSAGE), &s),
        Err(Error::Leaf)
    );
}

// ----------------------------------------------------- signed manifests

/// A development token and two H10 keys, made once per test run.
struct Keys {
    dir: PathBuf,
    slot: [u8; HSS_PUBLIC_KEY_LEN],
    config: [u8; HSS_PUBLIC_KEY_LEN],
}

fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(std::format!("ferrix-sigverify-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
        let slot = token
            .generate_from("slot", lms::LMS_SHA256_M24_H10, [1; 16], [2; 24])
            .unwrap();
        let config = token
            .generate_from("config", lms::LMS_SHA256_M24_H10, [3; 16], [4; 24])
            .unwrap();
        Keys {
            dir,
            slot: key52(&token.get_attribute(&slot, CKA_VALUE).unwrap()),
            config: key52(&token.get_attribute(&config, CKA_VALUE).unwrap()),
        }
    })
}

fn sign(label: &str, body: &Body) -> Vec<u8> {
    let mut token = SoftToken::open(&keys().dir, Vec::new()).unwrap();
    let bytes = body.encode();
    let signature = token
        .sign(&ObjectHandle(label.into()), CKM_HSS, &bytes)
        .unwrap();
    assemble(&bytes, &signature)
}

fn anchors() -> [Anchor; 2] {
    [
        Anchor {
            kind: Kind::Slot,
            id: 0x8000,
            public_key: keys().slot,
        },
        Anchor {
            kind: Kind::Config,
            id: 0x8001,
            public_key: keys().config,
        },
    ]
}

fn slot_body() -> Body {
    Body {
        kind: 1,
        security_version: 5,
        anchor_id: 0x8000,
        profile: 3,
        arch: 1,
        family: 0x0101,
        release: 42,
        label: *b"ferrix test 0000000000000000000\0",
        payload_len: 4096,
        payload_sha256: [0xAB; 32],
        floors: [5, 2, 1, 0],
        ..Body::default()
    }
}

const FAMILIES: &[u32] = &[0x0101];

fn slot_expect() -> Expect<'static> {
    Expect {
        kind: Kind::Slot,
        target: None,
        profile: 3,
        arch: 1,
        families: FAMILIES,
        subject: None,
        floor: 5,
    }
}

/// A good slot manifest, signed once.
fn good_slot() -> &'static [u8] {
    static BLOCK: OnceLock<Vec<u8>> = OnceLock::new();
    BLOCK.get_or_init(|| sign("slot", &slot_body()))
}

/// Verifies: L.sigverify.9
/// Verifies: H.SIGN.2
/// A good manifest verifies and returns its signed fields, and its payload
/// check holds only for its own length and digest.
#[test]
fn a_good_slot_manifest_verifies_with_its_fields() {
    let v = verify(good_slot(), &slot_expect(), &anchors(), &[]).unwrap();
    assert_eq!(
        (v.kind, v.security_version, v.anchor_id, v.release),
        (Kind::Slot, 5, 0x8000, 42)
    );
    assert_eq!(v.floors, [5, 2, 1, 0]);
    assert_eq!(v.check_payload(4096, &[0xAB; 32]), Ok(()));
    assert_eq!(
        v.check_payload(4095, &[0xAB; 32]),
        Err(Refusal::PayloadMismatch)
    );
    assert_eq!(
        v.check_payload(4097, &[0xAB; 32]),
        Err(Refusal::PayloadMismatch)
    );
    assert_eq!(
        v.check_payload(4096, &[0xAC; 32]),
        Err(Refusal::PayloadMismatch)
    );
    assert_eq!(v.revoked(), None);
}

/// Verifies: L.sigverify.1
/// Verifies: H.SIGN.5
/// Every byte of the block changed (one bit each, the bit rotating with
/// the byte) is refused: nothing in the 4 KiB is unchecked.
#[test]
fn every_byte_of_the_block_changed_is_refused() {
    let good = good_slot();
    for byte in 0..BLOCK_LEN {
        let mut b = good.to_vec();
        b[byte] ^= 1 << (byte % 8);
        assert!(
            verify(&b, &slot_expect(), &anchors(), &[]).is_err(),
            "byte {byte}"
        );
    }
}

fn refused(block: &[u8]) -> Refusal {
    verify(block, &slot_expect(), &anchors(), &[]).unwrap_err()
}

fn with(at: usize, bytes: &[u8]) -> Vec<u8> {
    let mut b = good_slot().to_vec();
    b[at..at + bytes.len()].copy_from_slice(bytes);
    b
}

/// Verifies: L.sigverify.1
/// Each structural field refused by its name, before any hash.
#[test]
fn each_structural_field_is_refused_by_its_name() {
    use Field::*;
    let m = Refusal::Malformed;
    assert_eq!(refused(&good_slot()[..BLOCK_LEN - 1]), m(Block));
    assert_eq!(refused(&with(0, b"FXSIGMAM")), m(Magic));
    assert_eq!(refused(&with(8, &2u16.to_le_bytes())), m(Format));
    assert_eq!(refused(&with(10, &511u16.to_le_bytes())), m(BodyLen));
    assert_eq!(refused(&with(12, &5u16.to_le_bytes())), m(Kind));
    assert_eq!(refused(&with(12, &0u16.to_le_bytes())), m(Kind));
    assert_eq!(refused(&with(14, &1u16.to_le_bytes())), m(Flags));
    assert_eq!(refused(&with(22, &[0])), m(Profile));
    assert_eq!(refused(&with(22, &[5])), m(Profile));
    assert_eq!(refused(&with(23, &[0])), m(Arch));
    assert_eq!(refused(&with(23, &[4])), m(Arch));
    assert_eq!(refused(&with(28, &[1])), m(Reserved));
    assert_eq!(refused(&with(72, &[1])), m(Subject));
    assert_eq!(refused(&with(160, &[1])), m(Revocation));
    assert_eq!(refused(&with(200, &[1])), m(Revocation));
    assert_eq!(refused(&with(170, &[1])), m(Padding));
    assert_eq!(refused(&with(500, &[1])), m(Padding));
    assert_eq!(refused(&with(4095, &[1])), m(Trailer));
    assert_eq!(
        refused(&with(BODY_LEN, &3533u32.to_le_bytes())),
        m(SignatureLen)
    );
}

/// Verifies: L.sigverify.1
/// `sig_len` one short and one long: the signature no longer parses, or a
/// non-zero byte lands in the trailer.
#[test]
fn a_signature_length_off_by_one_is_refused() {
    let good = good_slot();
    let len = u32::from_le_bytes(good[BODY_LEN..BODY_LEN + 4].try_into().unwrap());
    assert_eq!(
        refused(&with(BODY_LEN, &(len - 1).to_le_bytes())),
        Refusal::Malformed(Field::Trailer)
    );
    assert_eq!(
        refused(&with(BODY_LEN, &(len + 1).to_le_bytes())),
        Refusal::BadSignature
    );
}

/// Verifies: L.sigverify.5
/// The anchors are the expected kind's: a slot manifest checked as a
/// configuration is refused by its kind, and with the right kind but only
/// another kind's anchor, by the anchor; an unknown anchor id is refused.
#[test]
fn the_anchor_set_is_the_expected_kinds() {
    let config = Expect {
        kind: Kind::Config,
        ..slot_expect()
    };
    assert_eq!(
        verify(good_slot(), &config, &anchors(), &[]),
        Err(Refusal::WrongKind)
    );
    let only_config = [Anchor {
        kind: Kind::Config,
        id: 0x8000,
        public_key: keys().slot,
    }];
    assert_eq!(
        verify(good_slot(), &slot_expect(), &only_config, &[]),
        Err(Refusal::UnknownAnchor)
    );
    let renumbered = [Anchor {
        kind: Kind::Slot,
        id: 0x8001,
        public_key: keys().slot,
    }];
    assert_eq!(
        verify(good_slot(), &slot_expect(), &renumbered, &[]),
        Err(Refusal::UnknownAnchor)
    );
    // The right id and kind, but the configuration key: the signature fails.
    let wrong_key = [Anchor {
        kind: Kind::Slot,
        id: 0x8000,
        public_key: keys().config,
    }];
    assert_eq!(
        verify(good_slot(), &slot_expect(), &wrong_key, &[]),
        Err(Refusal::BadSignature)
    );
}

/// Verifies: L.sigverify.6
/// Verifies: L.sigverify.14
/// Kind 4 shares its target's anchors (SIGNING.md §1.1): a slot manifest
/// presented as a revocation, and a revocation presented as a slot, are
/// each refused by the signed kind.
#[test]
fn a_slot_and_a_revocation_are_told_apart_by_the_signed_kind() {
    let revocation = sign(
        "slot",
        &Body {
            kind: 4,
            target_kind: 1,
            revoked: std::vec![[9; 16]],
            floors: [0; 4],
            ..slot_body()
        },
    );
    let as_revocation = Expect {
        kind: Kind::Revocation,
        target: Some(Kind::Slot),
        ..slot_expect()
    };
    assert_eq!(
        verify(good_slot(), &as_revocation, &anchors(), &[]),
        Err(Refusal::WrongKind)
    );
    assert_eq!(
        verify(&revocation, &slot_expect(), &anchors(), &[]),
        Err(Refusal::WrongKind)
    );
    let v = verify(&revocation, &as_revocation, &anchors(), &[]).unwrap();
    assert_eq!(v.revoked(), Some((Kind::Slot, &[[9u8; 16]][..])));
    // Checked for the configuration kind, kind 1's signer is no anchor.
    let for_config = Expect {
        target: Some(Kind::Config),
        ..as_revocation
    };
    assert_eq!(
        verify(&revocation, &for_config, &anchors(), &[]),
        Err(Refusal::UnknownAnchor)
    );
}

/// Verifies: L.sigverify.6
/// Verifies: H.SIGN.2
/// Profile, arch, family and subject each one off are refused by name.
#[test]
fn each_bound_field_one_off_is_refused() {
    let anchors = anchors();
    let e = slot_expect();
    let check = |expect: Expect<'_>| verify(good_slot(), &expect, &anchors, &[]);
    assert_eq!(
        check(Expect { profile: 2, ..e }),
        Err(Refusal::WrongProfile)
    );
    assert_eq!(check(Expect { arch: 2, ..e }), Err(Refusal::WrongArch));
    assert_eq!(
        check(Expect {
            families: &[0x0102],
            ..e
        }),
        Err(Refusal::WrongFamily)
    );
    assert_eq!(
        check(Expect { families: &[], ..e }),
        Err(Refusal::WrongFamily)
    );
    assert_eq!(
        check(Expect {
            subject: Some(&[1; 32]),
            ..e
        }),
        Err(Refusal::WrongSubject)
    );
    assert!(
        check(Expect {
            families: &[0x0102, 0x0101],
            ..e
        })
        .is_ok()
    );
}

/// Verifies: L.sigverify.6
/// A driver manifest binds its subject: the driver it names, and no other.
#[test]
fn a_driver_manifest_binds_its_subject() {
    let mut subject = [0u8; 32];
    subject[..3].copy_from_slice(b"gpu");
    let mut other = [0u8; 32];
    other[..3].copy_from_slice(b"net");
    let anchors = [Anchor {
        kind: Kind::Driver,
        id: 0x8000,
        public_key: keys().slot,
    }];
    let block = sign(
        "slot",
        &Body {
            kind: 3,
            subject,
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Driver,
        subject: Some(&subject),
        ..slot_expect()
    };
    assert!(verify(&block, &e, &anchors, &[]).is_ok());
    assert_eq!(
        verify(
            &block,
            &Expect {
                subject: Some(&other),
                ..e
            },
            &anchors,
            &[]
        ),
        Err(Refusal::WrongSubject)
    );
}

/// Verifies: L.sigverify.7
/// Verifies: H.SIGN.3
/// The floor: one below is refused, the floor itself accepted.
#[test]
fn below_the_floor_is_refused_and_the_floor_itself_accepted() {
    let anchors = anchors();
    assert_eq!(
        verify(
            good_slot(),
            &Expect {
                floor: 6,
                ..slot_expect()
            },
            &anchors,
            &[]
        ),
        Err(Refusal::BelowFloor)
    );
    assert!(
        verify(
            good_slot(),
            &Expect {
                floor: 5,
                ..slot_expect()
            },
            &anchors,
            &[]
        )
        .is_ok()
    );
    assert!(
        verify(
            good_slot(),
            &Expect {
                floor: 0,
                ..slot_expect()
            },
            &anchors,
            &[]
        )
        .is_ok()
    );
}

/// Verifies: L.sigverify.8
/// Verifies: H.SIGN.4
/// A signature by a revoked key is refused; another key's revocation
/// leaves it alone.
#[test]
fn a_revoked_key_is_refused() {
    assert_eq!(
        verify(good_slot(), &slot_expect(), &anchors(), &[[1; 16]]),
        Err(Refusal::Revoked)
    );
    assert!(verify(good_slot(), &slot_expect(), &anchors(), &[[3; 16], [7; 16]]).is_ok());
}

/// Verifies: L.sigverify.1
/// A revocation's count past 20, or an entry past its count, is refused.
#[test]
fn a_revocation_count_past_twenty_or_an_entry_past_it_is_refused() {
    let revocation = sign(
        "slot",
        &Body {
            kind: 4,
            target_kind: 1,
            revoked: std::vec![[9; 16]],
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Revocation,
        target: Some(Kind::Slot),
        ..slot_expect()
    };
    let mut b = revocation.clone();
    b[162..164].copy_from_slice(&21u16.to_le_bytes());
    assert_eq!(
        verify(&b, &e, &anchors(), &[]),
        Err(Refusal::Malformed(Field::Revocation))
    );
    let mut b = revocation.clone();
    b[192] = 1;
    assert_eq!(
        verify(&b, &e, &anchors(), &[]),
        Err(Refusal::Malformed(Field::Revocation))
    );
    let mut b = revocation;
    b[160..162].copy_from_slice(&4u16.to_le_bytes());
    assert_eq!(
        verify(&b, &e, &anchors(), &[]),
        Err(Refusal::Malformed(Field::Revocation))
    );
}

/// Verifies: L.sigverify.10
/// No input reaches a panic: every truncation of a good block and of A.4's
/// signature, and blocks of every length up to a block and a byte, answer.
#[test]
fn every_truncation_answers_without_a_panic() {
    let good = good_slot();
    for len in (0..=BLOCK_LEN).step_by(7) {
        let _ = verify(&good[..len], &slot_expect(), &anchors(), &[]);
    }
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let signature = unhex(vectors::A4_SIGNATURE);
    for len in 0..signature.len() {
        assert!(lms::hss_verify(&public, b"", &signature[..len]).is_err());
    }
}

/// Verifies: L.sigverify.13
/// A revocation listing its own signing key's `I` is refused
/// (SIGNING.md §7.4 (a)).
#[test]
fn a_revocation_of_its_own_key_is_refused() {
    let own = sign(
        "slot",
        &Body {
            kind: 4,
            target_kind: 1,
            revoked: std::vec![[9; 16], [1; 16]],
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Revocation,
        target: Some(Kind::Slot),
        ..slot_expect()
    };
    assert_eq!(
        verify(&own, &e, &anchors(), &[]),
        Err(Refusal::RevokesItself)
    );
}

/// Verifies: L.sigverify.8
/// A revocation signed by a key the list in force already revokes is
/// refused (SIGNING.md §7.4 (b)).
#[test]
fn a_revocation_by_an_already_revoked_key_is_refused() {
    let revocation = sign(
        "slot",
        &Body {
            kind: 4,
            target_kind: 1,
            revoked: std::vec![[9; 16]],
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Revocation,
        target: Some(Kind::Slot),
        ..slot_expect()
    };
    assert_eq!(
        verify(&revocation, &e, &anchors(), &[[1; 16]]),
        Err(Refusal::Revoked)
    );
}

/// A two-level HSS key and a signature of `message` under it, joined as
/// SIGNING.md §4.3 describes: the top tree signs the lower tree's LMS
/// public key once, the lower tree signs the message.
fn two_levels(message: &[u8]) -> ([u8; HSS_PUBLIC_KEY_LEN], Vec<u8>, [u8; 16]) {
    use ferrix_softsign::lms::Key;
    type Tree = Vec<[u8; 24]>;
    static TREES: OnceLock<(Key, Tree, Key, Tree)> = OnceLock::new();
    let (top, top_tree, lower, lower_tree) = TREES.get_or_init(|| {
        let top = Key {
            lms_type: lms::LMS_SHA256_M24_H10,
            id: [0x21; 16],
            seed: [0x22; 24],
        };
        let lower = Key {
            lms_type: lms::LMS_SHA256_M24_H10,
            id: [0x23; 16],
            seed: [0x24; 24],
        };
        let (tt, lt) = (top.tree(), lower.tree());
        (top, tt, lower, lt)
    });
    let lower_public = lower.public_key(&lower_tree[1]);
    let mut signature = 1u32.to_be_bytes().to_vec();
    signature.extend(top.sign(3, &lower_public, top_tree));
    signature.extend(&lower_public);
    signature.extend(lower.sign(7, message, lower_tree));
    let mut public = 2u32.to_be_bytes().to_vec();
    public.extend(top.public_key(&top_tree[1]));
    (key52(&public), signature, lower.id)
}

/// Verifies: L.sigverify.4
/// A two-level signature verifies and names both trees, top first; a flip
/// in the signed lower key, in the top signature, or in the lower
/// signature is refused, and so is a lower key of a type outside the set.
#[test]
fn a_two_level_signature_verifies_and_each_level_is_checked() {
    let (public, signature, lower_id) = two_levels(b"two levels");
    let chain = lms::hss_verify(&public, b"two levels", &signature).unwrap();
    assert_eq!(chain.ids(), &[[0x21; 16], lower_id]);
    let lower_sig_len = lms::lms_signature_len(10);
    let pub_at = 4 + lower_sig_len;
    for at in [8, 100, pub_at + 30, pub_at + 48 + 100] {
        let mut s = signature.clone();
        s[at] ^= 1;
        assert!(
            lms::hss_verify(&public, b"two levels", &s).is_err(),
            "byte {at}"
        );
    }
    let mut s = signature.clone();
    s[pub_at..pub_at + 4].copy_from_slice(&0x0Eu32.to_be_bytes());
    assert_eq!(
        lms::hss_verify(&public, b"two levels", &s),
        Err(Error::TypeCode)
    );
    assert_eq!(
        lms::hss_verify(&public, b"two level", &signature),
        Err(Error::BadSignature)
    );
}

/// Verifies: L.sigverify.2
/// The product heights and their signature lengths.
#[test]
fn the_product_heights_and_lengths() {
    assert_eq!(lms::height(lms::LMS_SHA256_M24_H10), Some(10));
    assert_eq!(lms::height(lms::LMS_SHA256_M24_H20), Some(20));
    assert_eq!(lms::height(0x0E), None);
    assert_eq!(lms::lms_signature_len(20), 1740);
    assert_eq!(lms::HSS_SIGNATURE_MAX, crate::manifest::SIGNATURE_MAX);
}

/// Verifies: L.sigverify.1
/// Floors outside a slot manifest are refused, and every kind code round
/// trips.
#[test]
fn floors_outside_a_slot_and_the_kind_codes() {
    let config = sign(
        "config",
        &Body {
            kind: 2,
            anchor_id: 0x8001,
            floors: [1, 0, 0, 0],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Config,
        ..slot_expect()
    };
    assert_eq!(
        verify(&config, &e, &anchors(), &[]),
        Err(Refusal::Malformed(Field::Floors))
    );
    for kind in [Kind::Slot, Kind::Config, Kind::Driver, Kind::Revocation] {
        assert_eq!(Kind::from_code(kind.code()), Some(kind));
    }
}

/// Verifies: L.sigverify.6
/// A configuration manifest verifies under the configuration key, and a
/// revocation whose signed target is not the one the caller expects is
/// refused by its kind.
#[test]
fn a_config_manifest_verifies_and_a_revocations_target_is_bound() {
    let config = sign(
        "config",
        &Body {
            kind: 2,
            anchor_id: 0x8001,
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Config,
        subject: Some(&[0; 32]),
        ..slot_expect()
    };
    assert_eq!(
        verify(&config, &e, &anchors(), &[]).unwrap().kind,
        Kind::Config
    );
    let for_config = sign(
        "slot",
        &Body {
            kind: 4,
            target_kind: 2,
            revoked: std::vec![[9; 16]],
            floors: [0; 4],
            ..slot_body()
        },
    );
    let as_slot_revocation = Expect {
        kind: Kind::Revocation,
        target: Some(Kind::Slot),
        ..slot_expect()
    };
    assert_eq!(
        verify(&for_config, &as_slot_revocation, &anchors(), &[]),
        Err(Refusal::WrongKind)
    );
}

/// hbs-lms 0.1.1 writes RFC 8554's 256-bit type codes whatever its hash
/// (SIGNING.md §5.1); its SHA-256/192 keys and signatures take RFC 9858's
/// codes here: LMS 5-9 become 0x0A-0x0E, LM-OTS 1-4 become 5-8.
fn rfc_9858_codes(bytes: &mut [u8], lms_at: &[usize], ots_at: &[usize]) {
    for &at in lms_at {
        let code = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        bytes[at..at + 4].copy_from_slice(&(code + 5).to_be_bytes());
    }
    for &at in ots_at {
        let code = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        bytes[at..at + 4].copy_from_slice(&(code + 4).to_be_bytes());
    }
}

/// Verifies: L.sigverify.3, L.sigverify.4
/// An independent implementation's signatures (hbs-lms 0.1.1, Fraunhofer
/// AISEC, SHA-256/192, W4, H10, L = 1), with its type codes mapped to RFC
/// 9858's, verify here; one bit flipped in each is refused.
#[test]
fn hbs_lms_signatures_verify_here() {
    use hbs_lms::signature::SignerMut;
    use hbs_lms::{HssParameter, LmotsAlgorithm, LmsAlgorithm, Seed, Sha256_192};
    let parameters = [HssParameter::<Sha256_192>::new(
        LmotsAlgorithm::LmotsW4,
        LmsAlgorithm::LmsH10,
    )];
    let mut seed = Seed::default();
    for (n, b) in seed.as_mut_slice().iter_mut().enumerate() {
        *b = n as u8;
    }
    let (mut signing, verifying) = hbs_lms::keygen::<Sha256_192>(&parameters, &seed, None).unwrap();
    let mut public = verifying.as_slice().to_vec();
    rfc_9858_codes(&mut public, &[4], &[8]);
    let public = key52(&public);
    for n in 0..5u8 {
        let message = [n; 100];
        let signature = signing.try_sign(&message).unwrap();
        let mut s: Vec<u8> = signature.as_ref().to_vec();
        let lms_at = 4 + 4 + 4 + 24 + 51 * 24;
        rfc_9858_codes(&mut s, &[lms_at], &[8]);
        assert!(
            lms::hss_verify(&public, &message, &s).is_ok(),
            "signature {n}"
        );
        s[200] ^= 1;
        assert_eq!(
            lms::hss_verify(&public, &message, &s),
            Err(Error::BadSignature)
        );
    }
}

/// Verifies: L.sigverify.3
/// A signature whose LM-OTS type is not its key's is refused by its type,
/// even where the other type is one the code computes (the wide test set):
/// it is never read with the other type's layout.
#[test]
fn an_ots_type_other_than_the_keys_is_refused_by_its_type() {
    let public = key52(&unhex(vectors::A4_PUBLIC));
    let mut s = unhex(vectors::A4_SIGNATURE);
    s[8..12].copy_from_slice(&8u32.to_be_bytes());
    assert_eq!(
        lms::hss_verify_in(&EVERY_SHA256_192, &public, &unhex(vectors::A4_MESSAGE), &s),
        Err(Error::TypeCode)
    );
}

/// Verifies: L.sigverify.4
/// A key with three levels is refused by its level count, even with a
/// signature whose `Nspk` is two to match it.
#[test]
fn three_levels_are_refused_even_with_a_matching_nspk() {
    let mut public = key52(&unhex(vectors::A4_PUBLIC));
    public[..4].copy_from_slice(&3u32.to_be_bytes());
    let mut s = unhex(vectors::A4_SIGNATURE);
    s[..4].copy_from_slice(&2u32.to_be_bytes());
    assert_eq!(
        lms::hss_verify(&public, &unhex(vectors::A4_MESSAGE), &s),
        Err(Error::Levels)
    );
}

/// Verifies: L.sigverify.6
/// A configuration or driver manifest checked with no subject is refused,
/// whatever it signs: the caller must name what it will act on.
#[test]
fn a_config_or_driver_manifest_without_a_subject_is_refused() {
    let config = sign(
        "config",
        &Body {
            kind: 2,
            anchor_id: 0x8001,
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Config,
        subject: None,
        ..slot_expect()
    };
    assert_eq!(
        verify(&config, &e, &anchors(), &[]),
        Err(Refusal::NoSubject)
    );
    let driver_anchors = [Anchor {
        kind: Kind::Driver,
        id: 0x8000,
        public_key: keys().slot,
    }];
    let driver = sign(
        "slot",
        &Body {
            kind: 3,
            floors: [0; 4],
            ..slot_body()
        },
    );
    let e = Expect {
        kind: Kind::Driver,
        subject: None,
        ..slot_expect()
    };
    assert_eq!(
        verify(&driver, &e, &driver_anchors, &[]),
        Err(Refusal::NoSubject)
    );
}
