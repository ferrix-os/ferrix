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
#[cfg(unix)]
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

// ------------------------------------------------- the CI key (§6.5)

mod ci_key {
    use super::{LMS_SHA256_M24_H10, scratch, verifier};
    use crate::ci::{self, CiError, RunContext, Signer, leaf_range};
    use crate::lms::Key;

    fn ctx() -> RunContext {
        RunContext {
            run_number: 1,
            run_attempt: 2,
            workflow: ".github/workflows/signing-ci.yml".to_owned(),
        }
    }

    fn key() -> Key {
        Key {
            lms_type: LMS_SHA256_M24_H10,
            id: [0x5A; 16],
            seed: [0xC3; 24],
        }
    }

    /// A run attempt's 16 leaves are a pure function of its run number and
    /// attempt; attempt 0 or 5, run 0, and a range past the key are refused
    /// (ledger 4005 (3)).
    #[test]
    fn a_run_attempt_owns_sixteen_leaves_and_no_fifth_attempt_signs() {
        assert_eq!(leaf_range(1, 1, 1 << 20), Ok(64..80));
        assert_eq!(leaf_range(1, 4, 1 << 20), Ok(112..128));
        assert_eq!(leaf_range(2, 1, 1 << 20), Ok(128..144));
        assert_eq!(leaf_range(1, 5, 1 << 20), Err(CiError::Attempt));
        assert_eq!(leaf_range(1, 0, 1 << 20), Err(CiError::Attempt));
        assert_eq!(leaf_range(0, 1, 1 << 20), Err(CiError::Attempt));
        assert_eq!(leaf_range(16_383, 4, 1 << 20), Ok(1_048_560..1_048_576));
        assert_eq!(leaf_range(16_384, 1, 1 << 20), Err(CiError::Exhausted));
        assert_eq!(leaf_range(u64::MAX, 1, 1 << 20), Err(CiError::Exhausted));
    }

    /// The secret text round-trips, and anything else is refused.
    #[test]
    fn the_secret_text_round_trips() {
        let text = ci::render_secret(&key());
        assert_eq!(ci::parse_secret(&text), Ok(key()));
        assert!(ci::parse_secret(&text.replace("FXCIKEY-1", "FXCIKEY-2")).is_err());
        assert!(ci::parse_secret(&text.replace("0x0000000b", "0x0000000e")).is_err());
        assert!(ci::parse_secret(&text[..text.len() - 2]).is_err());
        assert!(ci::parse_secret("").is_err());
    }

    /// Sixteen signatures from one range, each verified, each leaf logged
    /// once; the seventeenth is refused (ledger 4005 (3)).
    #[test]
    fn a_range_signs_sixteen_times_and_then_refuses() {
        let dir = scratch();
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("run.log");
        let range = leaf_range(1, 2, 1024).unwrap();
        let mut signer = Signer::open(key(), range.clone(), "", &log, &[], &ctx()).unwrap();
        let public = signer.public_key();
        for n in 0..16u8 {
            let message = [n; 512];
            let signature = signer.sign(&message).unwrap();
            let public: [u8; 52] = public.clone().try_into().unwrap();
            assert!(
                verifier::hss_verify(&public, &message, &signature).is_ok(),
                "signature {n}"
            );
        }
        assert_eq!(signer.sign(&[0; 512]), Err(CiError::RangeUsed));
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(ci::audit(&text), Ok(16));
        let qs: Vec<u32> = text
            .lines()
            .map(|line| line.split_whitespace().nth(1).unwrap().parse().unwrap())
            .collect();
        assert_eq!(qs, range.collect::<Vec<_>>());
    }

    /// A range an earlier run's log already used is refused before anything
    /// is signed: a renamed workflow, a second one or a reset run number.
    #[test]
    fn a_range_an_earlier_log_used_is_refused() {
        let dir = scratch();
        let log = dir.join("run.log");
        let id = "5a".repeat(16);
        let other = "a5".repeat(16);
        let digest = "00".repeat(32);
        let used = format!("{id} 70 {digest}\n");
        assert_eq!(
            Signer::open(key(), 64..80, &used, &log, &[], &ctx()).map(|_| ()),
            Err(CiError::Overlap(70))
        );
        assert!(Signer::open(key(), 80..96, &used, &log, &[], &ctx()).is_ok());
        assert!(
            Signer::open(
                key(),
                64..80,
                &format!("{other} 70 {digest}\n"),
                &log,
                &[],
                &ctx()
            )
            .is_ok()
        );
        assert!(!log.exists());
    }

    /// Every `(I, q)` twice in the logs is refused (ledger 4005 (4)).
    #[test]
    fn a_leaf_logged_twice_is_refused() {
        let id = "5a".repeat(16);
        let digest = "11".repeat(32);
        let logs = format!("{id} 64 {digest} 1 1 w\n{id} 65 {digest}\n");
        assert_eq!(ci::audit(&logs), Ok(2));
        let planted = format!("{logs}{id} 64 {digest}\n");
        assert_eq!(ci::audit(&planted), Err(CiError::Repeated(id, 64)));
        assert!(ci::audit("not a log line").is_err());
    }

    /// No signature exists unless its leaf was logged and synced first (D2):
    /// a log that cannot be written refuses the signature.
    #[test]
    fn no_signature_without_its_leaf_logged_first() {
        let dir = scratch();
        let log = dir.join("absent").join("run.log");
        let mut signer = Signer::open(key(), 64..80, "", &log, &[], &ctx()).unwrap();
        assert!(matches!(signer.sign(&[1; 512]), Err(CiError::Log(_))));
    }

    /// A release key is refused (D1).
    #[test]
    fn a_release_key_is_refused_in_ci() {
        let dir = scratch();
        let log = dir.join("run.log");
        let public = Signer::open(key(), 64..80, "", &log, &[], &ctx())
            .unwrap()
            .public_key();
        assert_eq!(
            Signer::open(key(), 64..80, "", &log, &[public], &ctx()).map(|_| ()),
            Err(CiError::ReleaseKey)
        );
    }

    /// The signer's Debug never shows the key.
    #[test]
    fn a_signer_never_prints_its_key() {
        let dir = scratch();
        let log = dir.join("run.log");
        let signer = Signer::open(key(), 64..80, "", &log, &[], &ctx()).unwrap();
        let shown = format!("{signer:?}");
        assert!(!shown.contains("c3c3") && !shown.contains("195"), "{shown}");
    }
}

mod ci_anchors {
    use crate::ci::{self, CiError};

    const FILE: &str = r#"
# [release.*]      product keys
[development.slot]
kind = "slot"
anchor_id = 32768
public_key = "00"
made = "nazuna"

[development.ci-slot-1]
kind = "slot"
anchor_id = 36864
public_key = "0a0b"
made = "CI"
created = "2026-10-25"
expires = "2027-01-18"
workflow = ".github/workflows/signing-ci.yml"
"#;

    /// The CI anchor's key and expiry are read, with the release keys
    /// (none yet); another id or a non-CI key is refused (ledger 4188 (4),
    /// (5)).
    #[test]
    fn the_ci_anchor_is_read_from_the_anchors_file() {
        let (release, anchor) = ci::read_anchors(FILE, 36864).unwrap();
        assert!(release.is_empty());
        assert_eq!(anchor.public_key, vec![0x0a, 0x0b]);
        assert_eq!(anchor.expires, "2027-01-18");
        assert!(matches!(
            ci::read_anchors(FILE, 36865),
            Err(CiError::Anchors(_))
        ));
        assert!(matches!(
            ci::read_anchors(FILE, 32768),
            Err(CiError::Anchors(_))
        ));
        let with_release = format!(
            "[release.slot-0]\nkind = \"slot\"\nanchor_id = 0\npublic_key = \"ff\"\n{FILE}"
        );
        assert_eq!(
            ci::read_anchors(&with_release, 36864).unwrap().0,
            vec![vec![0xff]]
        );
    }

    /// ci-sign does not run without the anchors file (ledger 4188 (5)).
    #[test]
    fn ci_sign_needs_the_anchors_file() {
        assert!(matches!(
            ci::anchors_from(None, 36864),
            Err(CiError::Anchors(_))
        ));
        assert!(matches!(
            ci::anchors_from(Some("/nonexistent/signing-anchors.toml"), 36864),
            Err(CiError::Anchors(_))
        ));
        let real = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../data/signing-anchors.toml"
        );
        let (release, anchor) = ci::anchors_from(Some(real), 36864).unwrap();
        assert!(release.is_empty());
        assert_eq!(anchor.public_key.len(), 52);
    }

    /// An expired key signs nothing: on its expires day and after.
    #[test]
    fn an_expired_ci_key_is_refused() {
        let (_, anchor) = ci::read_anchors(FILE, 36864).unwrap();
        assert!(ci::refuse_expired(&anchor, "2027-01-17").is_ok());
        assert_eq!(
            ci::refuse_expired(&anchor, "2027-01-18"),
            Err(CiError::Expired("2027-01-18".to_owned()))
        );
        assert!(ci::refuse_expired(&anchor, "2027-02-01").is_err());
        assert_eq!(ci::day_of(0), "1970-01-01");
        assert_eq!(ci::day_of(20_751), "2026-10-25");
        assert_eq!(ci::today().len(), 10);
    }
}
