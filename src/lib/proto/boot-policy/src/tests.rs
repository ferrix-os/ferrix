//! Tests of the loader's decision (`docs/SIGNING.md` §10.6), on slots the
//! development soft signer signs.
//!
//! Each test's `Verifies:` line names the requirement it checks.

extern crate std;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::vec;
use std::vec::Vec;

use ferrix_sigverify::lms::{self, HSS_PUBLIC_KEY_LEN};
use ferrix_sigverify::{Anchor, Field, Kind};
use ferrix_slots::slot::{HEADER_LEN, SlotError, SlotHeader, VERSION_1, VERSION_2};
use ferrix_softsign::manifest::{Body, assemble};
use ferrix_softsign::token::{CKA_VALUE, CKM_HSS, ObjectHandle, SoftToken, Token};

use crate::{
    BLOCK_LEN, BlockField, Build, ENTRIES_AT, ENTRY_LEN, MAX_ANCHORS, Mode, Refusal, Rescue,
    Signed, decide, encode_block, generic_family, parse_block, rescue,
};

type Sig = ferrix_sigverify::Refusal;

/// A development token and two H10 keys, made once per test run.
struct Keys {
    dir: PathBuf,
    slot: [u8; HSS_PUBLIC_KEY_LEN],
    other: [u8; HSS_PUBLIC_KEY_LEN],
}

fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let dir = std::env::temp_dir().join(std::format!(
            "ferrix-boot-policy-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
        let slot = token
            .generate_from("slot", lms::LMS_SHA256_M24_H10, [5; 16], [6; 24])
            .unwrap();
        let other = token
            .generate_from("other", lms::LMS_SHA256_M24_H10, [7; 16], [8; 24])
            .unwrap();
        let key = |h| {
            token
                .get_attribute(h, CKA_VALUE)
                .unwrap()
                .try_into()
                .unwrap()
        };
        Keys {
            slot: key(&slot),
            other: key(&other),
            dir,
        }
    })
}

const SLOT_ID: u16 = 0x8000;

fn anchor() -> Anchor {
    Anchor {
        kind: Kind::Slot,
        id: SLOT_ID,
        public_key: keys().slot,
    }
}

fn block(mode: Mode) -> [u8; BLOCK_LEN] {
    encode_block(mode, &[anchor()])
}

const X86: u8 = 1;
const PC: u32 = 0x0101;
const N150: u32 = 0x0102;
const DESKTOP: Build<'static> = Build {
    profile: 4,
    arch: X86,
    claimed: false,
    families: &[PC],
};

/// A slot image of `version`, with its manifest block when signed.
struct Image {
    header: SlotHeader,
    bytes: Vec<u8>,
}

impl Image {
    fn new(version: u16) -> Image {
        let header = SlotHeader::lay_out(version, 3, "policy test", &[0x7F; 5000], &[1; 300], b"x");
        let mut bytes = vec![0u8; header.needed() as usize];
        bytes[..HEADER_LEN].copy_from_slice(&header.encode());
        Image { header, bytes }
    }

    fn signed(body: &Body) -> Image {
        Image::signed_by("slot", body)
    }

    fn signed_by(key: &str, body: &Body) -> Image {
        let mut image = Image::new(VERSION_2);
        let mut body = body.clone();
        body.payload_sha256 = image.header.seal();
        let manifest = sign(key, &body);
        image.bytes[4096..8192].copy_from_slice(&manifest);
        image
    }

    fn page(&self) -> &[u8] {
        &self.bytes[..HEADER_LEN]
    }

    fn manifest(&self) -> Option<&[u8]> {
        (self.header.version == VERSION_2).then(|| &self.bytes[4096..8192])
    }

    fn decide(
        &self,
        block: &[u8],
        build: &Build<'_>,
        family: u32,
    ) -> Result<crate::Start, Refusal> {
        decide(
            block,
            build,
            family,
            0,
            self.page(),
            self.manifest(),
            1 << 20,
        )
    }
}

fn sign(label: &str, body: &Body) -> Vec<u8> {
    let mut token = SoftToken::open(&keys().dir, Vec::new()).unwrap();
    let bytes = body.encode();
    let signature = token
        .sign(&ObjectHandle(label.into()), CKM_HSS, &bytes)
        .unwrap();
    assemble(&bytes, &signature)
}

fn body() -> Body {
    Body {
        kind: 1,
        security_version: 3,
        anchor_id: SLOT_ID,
        profile: 4,
        arch: X86,
        family: PC,
        release: 1,
        label: *b"ferrix policy test 000000000000\0",
        payload_len: 4064,
        ..Body::default()
    }
}

fn good() -> &'static Image {
    static IMAGE: OnceLock<Image> = OnceLock::new();
    IMAGE.get_or_init(|| Image::signed(&body()))
}

// ------------------------------------------------------------- the block

/// Verifies: L.bootpolicy.1
#[test]
fn a_short_block_refuses_every_slot() {
    let b = block(Mode::Off);
    assert_eq!(
        Image::new(VERSION_1).decide(&b[..BLOCK_LEN - 1], &DESKTOP, PC),
        Err(Refusal::Block(BlockField::Short))
    );
    assert_eq!(rescue(&b[..10]), Err(Refusal::Block(BlockField::Short)));
}

/// Verifies: L.bootpolicy.1
#[test]
fn an_unknown_block_version_or_mode_refuses_every_slot() {
    let mut b = block(Mode::Off);
    b[0] ^= 1;
    assert_eq!(
        Image::new(VERSION_1).decide(&b, &DESKTOP, PC),
        Err(Refusal::Block(BlockField::Magic))
    );
    let mut b = block(Mode::Off);
    b[8] = 2;
    assert_eq!(
        Image::new(VERSION_1).decide(&b, &DESKTOP, PC),
        Err(Refusal::Block(BlockField::Version))
    );
    let mut b = block(Mode::Off);
    b[10] = 2;
    assert_eq!(
        Image::new(VERSION_1).decide(&b, &DESKTOP, PC),
        Err(Refusal::Block(BlockField::Mode))
    );
    assert_eq!(rescue(&b), Err(Refusal::Block(BlockField::Mode)));
}

/// Verifies: L.bootpolicy.1
#[test]
fn a_count_above_eight_refuses_every_slot() {
    let mut b = block(Mode::Off);
    b[11] = MAX_ANCHORS as u8 + 1;
    assert_eq!(
        good().decide(&b, &DESKTOP, PC),
        Err(Refusal::Block(BlockField::Count))
    );
    // Eight is the most, and is read.
    let eight = [anchor(); MAX_ANCHORS];
    assert_eq!(
        parse_block(&encode_block(Mode::Off, &eight))
            .unwrap()
            .anchors()
            .len(),
        8
    );
}

/// Verifies: L.bootpolicy.1
#[test]
fn non_zero_block_padding_refuses_every_slot() {
    // The reserved bytes, an unused entry, and the tail.
    for at in [
        12,
        15,
        ENTRIES_AT + ENTRY_LEN,
        ENTRIES_AT + 3 * ENTRY_LEN + 9,
        BLOCK_LEN - 1,
    ] {
        let mut b = block(Mode::Off);
        b[at] = 1;
        assert_eq!(
            good().decide(&b, &DESKTOP, PC),
            Err(Refusal::Block(BlockField::Padding)),
            "byte {at}"
        );
    }
}

/// Verifies: L.bootpolicy.1
#[test]
fn a_non_slot_anchor_kind_refuses_every_slot() {
    let config = Anchor {
        kind: Kind::Config,
        ..anchor()
    };
    let b = encode_block(Mode::Off, &[config]);
    assert_eq!(
        good().decide(&b, &DESKTOP, PC),
        Err(Refusal::Block(BlockField::Kind))
    );
}

// ------------------------------------------------------------ versions

/// Verifies: L.bootpolicy.2
#[test]
fn version_1_starts_unsigned_under_off_and_is_refused_under_required() {
    let image = Image::new(VERSION_1);
    let start = image.decide(&block(Mode::Off), &DESKTOP, PC).unwrap();
    assert_eq!(start.signed, None);
    assert_eq!(start.header, image.header);
    assert_eq!(
        image.decide(&block(Mode::Required), &DESKTOP, PC),
        Err(Refusal::Unsigned)
    );
}

/// Verifies: L.bootpolicy.3
#[test]
fn version_2_with_a_generation_is_refused() {
    let mut header = good().header;
    header.generation = 9;
    let page = header.encode();
    assert_eq!(
        decide(
            &block(Mode::Off),
            &DESKTOP,
            PC,
            0,
            &page,
            good().manifest(),
            1 << 20
        ),
        Err(Refusal::Header(SlotError::Generation))
    );
}

/// Verifies: L.bootpolicy.4
/// Verifies: H.SIGN.7
#[test]
fn version_2_is_verified_against_the_blocks_anchors() {
    for mode in [Mode::Off, Mode::Required] {
        let start = good().decide(&block(mode), &DESKTOP, PC).unwrap();
        assert_eq!(
            start.signed,
            Some(Signed {
                anchor_id: SLOT_ID,
                security_version: 3
            })
        );
    }
    // Another key's signature, under the same anchor id.
    let forged = Image::signed_by("other", &body());
    assert_eq!(
        forged.decide(&block(Mode::Off), &DESKTOP, PC),
        Err(Refusal::Manifest(Sig::BadSignature))
    );
    // A block that does not hold the anchor: an empty release table.
    assert_eq!(
        good().decide(&encode_block(Mode::Off, &[]), &DESKTOP, PC),
        Err(Refusal::Manifest(Sig::UnknownAnchor))
    );
    // The other key as an anchor of its own: still not the signer's.
    let other = Anchor {
        kind: Kind::Slot,
        id: SLOT_ID,
        public_key: keys().other,
    };
    assert_eq!(
        good().decide(&encode_block(Mode::Off, &[other]), &DESKTOP, PC),
        Err(Refusal::Manifest(Sig::BadSignature))
    );
    // A manifest of another kind, signed by the slot key.
    let config = Image::signed(&Body {
        kind: 2,
        subject: [1; 32],
        ..body()
    });
    assert_eq!(
        config.decide(&block(Mode::Off), &DESKTOP, PC),
        Err(Refusal::Manifest(Sig::WrongKind))
    );
}

/// Verifies: L.bootpolicy.5
#[test]
fn version_2_is_never_started_unverified() {
    // No manifest block given, a short one, and a zero one.
    let page = good().page();
    for manifest in [None, Some(&[0u8; 4095][..]), Some(&[0u8; 4096][..])] {
        let refused = decide(&block(Mode::Off), &DESKTOP, PC, 0, page, manifest, 1 << 20);
        assert!(matches!(refused, Err(Refusal::Manifest(_))), "{refused:?}");
    }
    // Every byte of the signed body changed, under off.
    for at in (0..512).step_by(5) {
        let mut bytes = good().bytes.clone();
        bytes[4096 + at] ^= 0x10;
        let refused = decide(
            &block(Mode::Off),
            &DESKTOP,
            PC,
            0,
            &bytes[..HEADER_LEN],
            Some(&bytes[4096..8192]),
            1 << 20,
        );
        assert!(refused.is_err(), "byte {at}");
    }
}

/// Verifies: L.bootpolicy.6
#[test]
fn the_manifest_must_cover_this_header() {
    // A good manifest moved onto another header (another kernel).
    let other = SlotHeader::lay_out(VERSION_2, 0, "policy test", &[0x7E; 5000], &[1; 300], b"x");
    let page = other.encode();
    assert_eq!(
        decide(
            &block(Mode::Off),
            &DESKTOP,
            PC,
            0,
            &page,
            good().manifest(),
            1 << 20
        ),
        Err(Refusal::Manifest(Sig::PayloadMismatch))
    );
    // The right digest with another length: the whole header page.
    let image = Image::signed(&Body {
        payload_len: 4096,
        ..body()
    });
    assert_eq!(
        image.decide(&block(Mode::Off), &DESKTOP, PC),
        Err(Refusal::Manifest(Sig::PayloadMismatch))
    );
}

/// Verifies: L.bootpolicy.7
#[test]
fn another_profile_or_arch_is_refused() {
    let present = Build {
        profile: 3,
        ..DESKTOP
    };
    assert_eq!(
        good().decide(&block(Mode::Off), &present, PC),
        Err(Refusal::Manifest(Sig::WrongProfile))
    );
    let arm = Build {
        arch: 3,
        families: &[PC, 0x0301],
        ..DESKTOP
    };
    assert_eq!(
        good().decide(&block(Mode::Off), &arm, 0x0301),
        Err(Refusal::Manifest(Sig::WrongArch))
    );
}

/// Verifies: L.bootpolicy.8
#[test]
fn families_follow_the_claim() {
    assert_eq!(generic_family(X86), PC);
    let n150_body = Body {
        family: N150,
        ..body()
    };
    let n150 = Image::signed(&n150_body);
    // Without a claim: the machine's own family, then the generic one.
    let desktop = Build {
        families: &[N150, PC],
        ..DESKTOP
    };
    assert!(n150.decide(&block(Mode::Off), &desktop, N150).is_ok());
    assert!(good().decide(&block(Mode::Off), &desktop, N150).is_ok());
    // An N150 manifest on a machine recognised only as generic.
    assert_eq!(
        n150.decide(&block(Mode::Off), &desktop, PC),
        Err(Refusal::Manifest(Sig::WrongFamily))
    );
    // With a claim of the N150 alone: no generic fallback, and an
    // unrecognised machine (generic) accepts nothing.
    let claimed = Build {
        profile: 4,
        claimed: true,
        families: &[N150],
        ..DESKTOP
    };
    assert!(n150.decide(&block(Mode::Off), &claimed, N150).is_ok());
    assert_eq!(
        good().decide(&block(Mode::Off), &claimed, N150),
        Err(Refusal::Manifest(Sig::WrongFamily))
    );
    assert_eq!(
        n150.decide(&block(Mode::Off), &claimed, PC),
        Err(Refusal::Manifest(Sig::WrongFamily))
    );
    // A claim that lists the generic family beside the N150's still gives
    // an N150 no generic fallback: its own family only.
    let both = Build {
        families: &[N150, PC],
        ..claimed
    };
    assert_eq!(
        good().decide(&block(Mode::Off), &both, N150),
        Err(Refusal::Manifest(Sig::WrongFamily))
    );
    assert!(good().decide(&block(Mode::Off), &both, PC).is_ok());
    // A family the profile does not list is never accepted.
    let none = Build {
        families: &[],
        ..DESKTOP
    };
    assert_eq!(
        good().decide(&block(Mode::Off), &none, PC),
        Err(Refusal::Manifest(Sig::WrongFamily))
    );
}

/// Verifies: L.bootpolicy.9
#[test]
fn the_floor_is_the_callers() {
    let page = good().page();
    let at = |floor| {
        decide(
            &block(Mode::Off),
            &DESKTOP,
            PC,
            floor,
            page,
            good().manifest(),
            1 << 20,
        )
    };
    assert!(at(0).is_ok());
    assert!(at(3).is_ok());
    assert_eq!(at(4), Err(Refusal::Manifest(Sig::BelowFloor)));
}

/// Verifies: L.bootpolicy.10
#[test]
fn the_rescue_path_follows_the_mode() {
    assert_eq!(rescue(&block(Mode::Off)), Ok(Rescue::UnsignedFiles));
    assert_eq!(rescue(&block(Mode::Required)), Ok(Rescue::SlotImage));
}

/// Verifies: L.bootpolicy.11
#[test]
fn start_names_the_anchor_and_version() {
    let start = good().decide(&block(Mode::Required), &DESKTOP, PC).unwrap();
    assert_eq!(start.header, good().header);
    let signed = start.signed.unwrap();
    assert_eq!((signed.anchor_id, signed.security_version), (SLOT_ID, 3));
}

/// Verifies: L.bootpolicy.12
/// Every input gives an answer: arbitrary headers and manifests, short and
/// long, never panic.
#[test]
fn every_input_gives_an_answer() {
    let mut seed = 0x1234_5678_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for _ in 0..300 {
        let mut bytes = good().bytes.clone();
        for _ in 0..8 {
            let at = (next() as usize) % 8192;
            bytes[at] = next() as u8;
        }
        let cut = (next() as usize) % 4200;
        for (page, manifest) in [
            (&bytes[..HEADER_LEN], Some(&bytes[4096..8192])),
            (
                &bytes[..cut.min(HEADER_LEN)],
                Some(&bytes[4096..4096 + cut.min(4096)]),
            ),
        ] {
            let _ = decide(&block(Mode::Off), &DESKTOP, PC, 0, page, manifest, 1 << 20);
        }
    }
    let _ = Field::Block;
}

/// Verifies: L.bootpolicy.1
/// The four blocks gen-anchors.py writes for the loader parse here, with
/// the mode their names give, and the development ones hold the
/// development slot anchor: the script and `encode_block` lay a block out
/// alike.
#[test]
fn the_generated_blocks_parse_as_named() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../boot/common/uefi/anchors");
    for (name, mode, development) in [
        ("release-off", Mode::Off, false),
        ("release-required", Mode::Required, false),
        ("development-off", Mode::Off, true),
        ("development-required", Mode::Required, true),
    ] {
        let bytes = std::fs::read(dir.join(std::format!("{name}.bin"))).unwrap();
        let policy = parse_block(&bytes).unwrap();
        assert_eq!(policy.mode, mode, "{name}");
        assert_eq!(
            policy.anchors().iter().any(|a| a.id == 0x8000),
            development,
            "{name}"
        );
        assert_eq!(
            encode_block(mode, policy.anchors())[..],
            bytes[..],
            "{name}"
        );
    }
}
