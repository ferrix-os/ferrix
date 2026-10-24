//! SHA-256 against FIPS 180-4's examples, and SHA-256/192 against RFC 9858
//! §2.1's.

#![expect(
    clippy::arithmetic_side_effects,
    clippy::string_slice,
    reason = "AUDIT: fixtures: small known lengths, and the hex sliced is NIST's, ASCII by construction"
)]

extern crate std;

use std::string::String;
use std::vec;
use std::vec::Vec;

use super::{Sha256, digest, digest_192, finish_192};

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut out = String::new();
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Verifies: L.sha256.1, L.sha256.2
/// FIPS 180-4's (and the NIST example values') messages: empty, "abc", the
/// two-block message, and a million `a`s, also fed in uneven pieces.
#[test]
fn sha256_matches_fips_180_4() {
    assert_eq!(
        hex(&digest(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    let million = vec![b'a'; 1_000_000];
    assert_eq!(
        hex(&digest(&million)),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
    let mut hash = Sha256::new();
    for piece in million.chunks(997) {
        hash.update(piece);
    }
    assert_eq!(hash.finish(), digest(&million));
}

/// Verifies: L.sha256.2
/// Every length from 0 to 130 bytes crosses the padding's two cases (55
/// and 56 bytes, and the block boundary); piecewise and whole agree.
#[test]
fn every_short_length_hashes_the_same_whole_and_bytewise() {
    let data: Vec<u8> = (0..130u8).collect();
    for len in 0..=data.len() {
        let whole = digest(data.get(..len).unwrap_or_default());
        let mut hash = Sha256::new();
        for byte in data.get(..len).unwrap_or_default() {
            hash.update(core::slice::from_ref(byte));
        }
        assert_eq!(hash.finish(), whole, "length {len}");
    }
}

/// Verifies: L.sha256.3
/// RFC 9858 §2.1's vector: SHA-256/192("abc") is SHA-256("abc") without its
/// last 8 bytes.
#[test]
fn sha256_192_matches_rfc_9858() {
    assert_eq!(
        hex(&digest_192(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9c"
    );
    let mut hash = Sha256::new();
    hash.update(b"ab");
    hash.update(b"c");
    assert_eq!(finish_192(hash), digest_192(b"abc"));
}

/// One `.rsp` file's `Len`/`Msg`/`MD` records (NIST CAVP SHAVS, byte
/// oriented, CAVS 11.0/11.1, `shabytetestvectors.zip`; line endings made
/// LF, nothing else changed).
fn records(rsp: &str) -> Vec<(usize, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let (mut len, mut msg) = (None, None);
    for line in rsp.lines() {
        if let Some(v) = line.strip_prefix("Len = ") {
            len = Some(v.parse::<usize>().unwrap());
        } else if let Some(v) = line.strip_prefix("Msg = ") {
            msg = Some(unhex(v));
        } else if let Some(v) = line.strip_prefix("MD = ") {
            let bits = len.take().unwrap();
            let mut m = msg.take().unwrap();
            m.truncate(bits / 8);
            out.push((bits, m, unhex(v)));
        }
    }
    out
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|n| u8::from_str_radix(&text[2 * n..2 * n + 2], 16).unwrap())
        .collect()
}

/// Verifies: L.sha256.1
/// NIST CAVP `SHA256ShortMsg`: all 65 messages, 0 to 64 bytes.
#[test]
fn nist_cavp_short_messages() {
    let cases = records(include_str!("cavp/SHA256ShortMsg.rsp"));
    assert_eq!(cases.len(), 65);
    for (bits, msg, md) in cases {
        assert_eq!(digest(&msg).to_vec(), md, "Len = {bits}");
    }
}

/// Verifies: L.sha256.1
/// NIST CAVP `SHA256LongMsg`: all 64 messages, up to 51,200 bits.
#[test]
fn nist_cavp_long_messages() {
    let cases = records(include_str!("cavp/SHA256LongMsg.rsp"));
    assert_eq!(cases.len(), 64);
    for (bits, msg, md) in cases {
        assert_eq!(digest(&msg).to_vec(), md, "Len = {bits}");
    }
}

/// Verifies: L.sha256.1
/// NIST CAVP `SHA256Monte`: SHAVS §6.4's Monte Carlo test, 100 checkpoints
/// of 1,000 chained hashes each.
#[test]
fn nist_cavp_monte_carlo() {
    let rsp = include_str!("cavp/SHA256Monte.rsp");
    let mut seed = unhex(rsp.lines().find_map(|l| l.strip_prefix("Seed = ")).unwrap());
    let expected: Vec<Vec<u8>> = rsp
        .lines()
        .filter_map(|l| l.strip_prefix("MD = "))
        .map(unhex)
        .collect();
    assert_eq!(expected.len(), 100);
    for md in expected {
        let (mut a, mut b, mut c) = (seed.clone(), seed.clone(), seed.clone());
        for _ in 3..1003 {
            let mut m = a.clone();
            m.extend_from_slice(&b);
            m.extend_from_slice(&c);
            let next = digest(&m).to_vec();
            a = b;
            b = c;
            c = next;
        }
        assert_eq!(c, md);
        seed = c;
    }
}

/// Verifies: L.sha256.2
/// `Sha256::default` is the empty hash `Sha256::new` is.
#[test]
fn the_default_hash_is_the_empty_hash() {
    assert_eq!(Sha256::default().finish(), digest(b""));
}
