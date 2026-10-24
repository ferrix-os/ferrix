//! Tests of the soft signer: RFC 9858 A.4's one-time signature reproduced
//! from its seed, every signature verified by `ferrix-sigverify`, and the
//! state rule of `docs/SIGNING.md` §6.4 (D1, D2, D3, D5).

#![expect(
    clippy::arithmetic_side_effects,
    clippy::string_slice,
    reason = "AUDIT: fixtures: small known lengths, and the hex sliced is ASCII by construction (the RFCs' and ACVP's vectors)"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use ferrix_sigverify::lms as verifier;

use crate::lms::{Key, LMS_SHA256_M24_H10, LMS_SHA256_M24_H20};
use crate::manifest::{BODY_LEN, Body, sign};
use crate::token::{
    CKA_HSS_KEYS_REMAINING, CKA_VALUE, CKM_HSS, CKM_HSS_KEY_PAIR_GEN, Error, ObjectHandle,
    SoftToken, Token,
};

#[path = "../../../../../src/lib/crypto/sigverify/src/vectors.rs"]
#[allow(
    dead_code,
    reason = "the soft signer's tests use only RFC 9858 A.4 of the shared vectors"
)]
mod vectors;

fn unhex(parts: &[&str]) -> Vec<u8> {
    let text: String = parts.concat();
    (0..text.len() / 2)
        .map(|n| u8::from_str_radix(&text[2 * n..2 * n + 2], 16).unwrap())
        .collect()
}

fn scratch() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ferrix-softsign-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// RFC 9858 A.4's private key reproduces its one-time signature for
/// `q = 100` byte for byte: `C` derived as the reference implementation
/// does, and every chain value. (The tree is 2^20 leaves, so the path is
/// the vector's and not recomputed here.)
#[test]
fn rfc_9858_a4_one_time_signature_is_reproduced_from_its_seed() {
    let private = unhex(vectors::A4_PRIVATE);
    let key = Key {
        lms_type: LMS_SHA256_M24_H20,
        seed: private[..24].try_into().unwrap(),
        id: private[24..40].try_into().unwrap(),
    };
    let ours = key.sign(100, &unhex(vectors::A4_MESSAGE), &[]);
    let theirs = unhex(vectors::A4_SIGNATURE);
    // HSS: Nspk (4), then the LMS signature: q (4) and the LM-OTS
    // signature (4 + 24 + 51 * 24).
    let ots_end = 4 + 4 + 4 + 24 + 51 * 24;
    assert_eq!(&ours[..ots_end - 4], &theirs[4..ots_end]);
}

/// A key's every signature verifies under `ferrix-sigverify`, its public
/// key is an HSS key with `L = 1`, and the remaining count falls by one
/// each time.
#[test]
fn every_signature_verifies_and_the_count_falls() {
    let mut token = SoftToken::open(&scratch(), Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [5; 16], [6; 24])
        .unwrap();
    let public: [u8; 52] = token
        .get_attribute(&key, CKA_VALUE)
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(&public[..4], &1u32.to_be_bytes());
    for n in 0..40u64 {
        let left = token.get_attribute(&key, CKA_HSS_KEYS_REMAINING).unwrap();
        assert_eq!(u64::from_be_bytes(left.try_into().unwrap()), 1024 - n);
        let message = format!("message {n}");
        let signature = token.sign(&key, CKM_HSS, message.as_bytes()).unwrap();
        assert_eq!(signature.len(), 4 + verifier::lms_signature_len(10));
        assert!(
            verifier::hss_verify(&public, message.as_bytes(), &signature).is_ok(),
            "signature {n}"
        );
    }
}

/// The manifest it writes is the block `ferrix-sigverify` reads (D3).
#[test]
fn a_signed_manifest_verifies() {
    let mut token = SoftToken::open(&scratch(), Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [7; 16], [8; 24])
        .unwrap();
    let public: [u8; 52] = token
        .get_attribute(&key, CKA_VALUE)
        .unwrap()
        .try_into()
        .unwrap();
    let body = Body {
        kind: 2,
        security_version: 3,
        anchor_id: 0x8001,
        profile: 2,
        arch: 2,
        family: 0x0201,
        ..Body::default()
    };
    let block = sign(&mut token, &key, &body).unwrap();
    assert_eq!(block.len(), 4096);
    assert_eq!(&block[..BODY_LEN], &body.encode());
    let anchors = [ferrix_sigverify::Anchor {
        kind: ferrix_sigverify::Kind::Config,
        id: 0x8001,
        public_key: public,
    }];
    let expect = ferrix_sigverify::Expect {
        kind: ferrix_sigverify::Kind::Config,
        target: None,
        profile: 2,
        arch: 2,
        families: &[0x0201],
        subject: Some(&[0; 32]),
        floor: 3,
    };
    let v = ferrix_sigverify::verify(&block, &expect, &anchors, &[]).unwrap();
    assert_eq!(v.security_version, 3);
}

/// (D2) A restored older state file is refused: its next leaf is not above
/// the log's.
#[test]
fn a_restored_older_state_is_refused() {
    let dir = scratch();
    let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [9; 16], [10; 24])
        .unwrap();
    let before = std::fs::read(dir.join("k/state")).unwrap();
    let _ = token.sign(&key, CKM_HSS, b"one").unwrap();
    let _ = token.sign(&key, CKM_HSS, b"two").unwrap();
    std::fs::write(dir.join("k/state"), before).unwrap();
    match token.sign(&key, CKM_HSS, b"three") {
        Err(Error::Reuse { next: 0, logged: 1 }) => {}
        other => panic!("expected a reuse refusal, got {other:?}"),
    }
}

/// (D2) A crash after the advanced state is committed and before the
/// signature leaves skips that leaf: the next signature uses the leaf
/// after it, never the same one.
#[test]
fn a_crash_after_the_commit_skips_the_leaf() {
    let dir = scratch();
    let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [11; 16], [12; 24])
        .unwrap();
    let _ = token.sign(&key, CKM_HSS, b"one").unwrap();
    // The crash: state says next 2, the log knows only leaf 0.
    let state = std::fs::read_to_string(dir.join("k/state"))
        .unwrap()
        .replace("next 1", "next 2");
    std::fs::write(dir.join("k/state"), state).unwrap();
    let signature = token.sign(&key, CKM_HSS, b"two").unwrap();
    assert_eq!(&signature[4..8], &2u32.to_be_bytes());
}

/// (D2) A state that cannot be read or parsed, or a key with every leaf
/// used, is refused.
#[test]
fn an_unreadable_or_exhausted_state_is_refused() {
    let dir = scratch();
    let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [13; 16], [14; 24])
        .unwrap();
    let good = std::fs::read_to_string(dir.join("k/state")).unwrap();
    std::fs::write(dir.join("k/state"), good.replace("next 0", "next 1024")).unwrap();
    assert!(matches!(
        token.sign(&key, CKM_HSS, b"x"),
        Err(Error::Exhausted)
    ));
    std::fs::write(dir.join("k/state"), "not a state").unwrap();
    assert!(matches!(
        token.sign(&key, CKM_HSS, b"x"),
        Err(Error::State(_))
    ));
    std::fs::remove_file(dir.join("k/state")).unwrap();
    assert!(matches!(
        token.sign(&key, CKM_HSS, b"x"),
        Err(Error::State(_))
    ));
}

/// (D2) A state directory that cannot be written: the advance fails, and
/// nothing is signed.
#[test]
fn a_state_that_cannot_be_advanced_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [15; 16], [16; 24])
        .unwrap();
    std::fs::set_permissions(dir.join("k"), std::fs::Permissions::from_mode(0o500)).unwrap();
    let answer = token.sign(&key, CKM_HSS, b"x");
    std::fs::set_permissions(dir.join("k"), std::fs::Permissions::from_mode(0o700)).unwrap();
    // Root can write anyway; where it cannot, the refusal is State.
    if std::fs::metadata("/root")
        .map(|m| m.permissions().readonly())
        .unwrap_or(true)
    {
        assert!(matches!(answer, Err(Error::State(_))), "{answer:?}");
    }
}

/// (D1) A key whose public key is a release anchor is never signed with.
#[test]
fn a_release_anchor_is_refused() {
    let dir = scratch();
    let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
    let key = token
        .generate_from("k", LMS_SHA256_M24_H10, [17; 16], [18; 24])
        .unwrap();
    let public = token.get_attribute(&key, CKA_VALUE).unwrap();
    let mut token = SoftToken::open(&dir, vec![public]).unwrap();
    assert!(matches!(
        token.sign(&key, CKM_HSS, b"x"),
        Err(Error::ReleaseKey)
    ));
}

/// (D5) A key directory inside a git work tree is refused.
#[test]
fn a_directory_inside_a_git_tree_is_refused() {
    let dir = scratch();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    assert!(matches!(
        SoftToken::open(&dir.join("keys"), Vec::new()),
        Err(Error::InRepository(_))
    ));
}

/// (D3) The subset only: another mechanism, `L = 2` or another LM-OTS
/// type is refused; an existing label is not overwritten.
#[test]
fn only_the_subset_is_offered() {
    let mut token = SoftToken::open(&scratch(), Vec::new()).unwrap();
    assert!(matches!(
        token.generate_key_pair(CKM_HSS, "a", 1, LMS_SHA256_M24_H10, 7),
        Err(Error::Mechanism(_))
    ));
    assert!(matches!(
        token.generate_key_pair(CKM_HSS_KEY_PAIR_GEN, "a", 2, LMS_SHA256_M24_H10, 7),
        Err(Error::Parameters)
    ));
    assert!(matches!(
        token.generate_key_pair(CKM_HSS_KEY_PAIR_GEN, "a", 1, LMS_SHA256_M24_H10, 8),
        Err(Error::Parameters)
    ));
    let key = token
        .generate_from("a", LMS_SHA256_M24_H10, [1; 16], [1; 24])
        .unwrap();
    assert!(matches!(
        token.generate_from("a", LMS_SHA256_M24_H10, [1; 16], [1; 24]),
        Err(Error::Exists(_))
    ));
    assert!(matches!(
        token.sign(&key, CKM_HSS_KEY_PAIR_GEN, b"x"),
        Err(Error::Mechanism(_))
    ));
    assert!(matches!(
        token.get_attribute(&ObjectHandle("nope".into()), CKA_VALUE),
        Err(Error::NoKey(_))
    ));
}

/// (D3) The PKCS #11 constants are pkcs11t.h 3.01's (OASIS, PKCS #11
/// Specification Version 3.1, `published/3-01/pkcs11t.h`:
/// `CKK_HSS 0x46`, `CKA_HSS_LEVELS 0x617`, `CKA_HSS_LMS_TYPE 0x618`,
/// `CKA_HSS_LMOTS_TYPE 0x619`, `CKA_HSS_KEYS_REMAINING 0x61c`,
/// `CKM_HSS_KEY_PAIR_GEN 0x4032`, `CKM_HSS 0x4033`; `CKA_VALUE 0x11`).
#[test]
fn the_pkcs11_constants_are_the_standards() {
    use crate::token::{CKA_HSS_LEVELS, CKA_HSS_LMOTS_TYPE, CKA_HSS_LMS_TYPE, CKK_HSS};
    assert_eq!(
        [
            CKK_HSS,
            CKA_HSS_LEVELS,
            CKA_HSS_LMS_TYPE,
            CKA_HSS_LMOTS_TYPE,
            CKA_HSS_KEYS_REMAINING,
            CKM_HSS_KEY_PAIR_GEN,
            CKM_HSS,
            CKA_VALUE
        ],
        [0x46, 0x617, 0x618, 0x619, 0x61c, 0x4032, 0x4033, 0x11]
    );
}
