//! Fuzz product signing's verifier (`docs/SIGNING.md` §8.5).
//!
//! Every block the loader, init or devmgr will ever check comes from
//! someone else: a disk, a package, a driver update. The contract is total:
//! for any bytes, `verify` returns a verified manifest or one named
//! refusal, and `hss_verify` an answer, with no panic, no overflow and no
//! read outside the slice. The seed in `corpus/sigverify_manifest` is a slot
//! manifest the development soft signer made with the key below, so the
//! search starts from a block that verifies and walks every check from
//! there.

#![no_main]

use ferrix_sigverify::{Anchor, Expect, Kind, lms, verify};
use libfuzzer_sys::fuzz_target;

/// The development key the seed was signed with (`ferrix-softsign keygen`,
/// `LMS_SHA256_M24_H10`; its private state was never in the tree).
const SEED_KEY: [u8; 52] = [
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x07, 0x06, 0x45, 0x93, 0xd7,
    0xea, 0xd3, 0xad, 0x5b, 0x30, 0x93, 0x70, 0x2f, 0xe5, 0x81, 0x9d, 0x4c, 0xd3, 0xdb, 0x83, 0xbf,
    0x2c, 0x76, 0x81, 0x1f, 0x07, 0xf2, 0xd1, 0xc0, 0x34, 0x2c, 0x9c, 0x2a, 0xd8, 0x5d, 0xbc, 0x23,
    0x94, 0x1d, 0xb4, 0x2f,
];

fuzz_target!(|data: &[u8]| {
    let anchors = [
        Anchor { kind: Kind::Slot, id: 0x8000, public_key: SEED_KEY },
        Anchor { kind: Kind::Config, id: 0x8000, public_key: SEED_KEY },
    ];
    for kind in [Kind::Slot, Kind::Config, Kind::Revocation] {
        let expect = Expect {
            kind,
            target: Some(Kind::Slot),
            profile: 3,
            arch: 1,
            families: &[0x0101],
            subject: (kind == Kind::Config).then_some(&[0u8; 32]),
            floor: 5,
        };
        if let Ok(v) = verify(data, &expect, &anchors, &[[0; 16]]) {
            let _ = v.check_payload(v.payload_len, &v.payload_sha256);
            let _ = v.revoked();
        }
    }
    // The signature path alone, on the raw bytes after a 4-byte prefix.
    let _ = lms::hss_verify(&SEED_KEY, data.get(..data.len().min(512)).unwrap_or(&[]), data);
});
