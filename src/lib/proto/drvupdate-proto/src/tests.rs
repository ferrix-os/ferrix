//! A driver update's messages (`docs/DEVMGR.md` §4.1): each round-trips,
//! and each refusal names what is wrong.

use crate::{
    ANSWER_BYTES, ANY, Answer, Fingerprint, MAX_IMAGE, Outcome, REQUEST_BYTES, Refused, Request,
};

#[test]
fn a_request_round_trips() {
    let request = Request::new(b"gpu", ANY, 123_456).expect("a name that fits");
    let bytes = request.encode();
    assert_eq!(bytes.len(), REQUEST_BYTES);
    assert_eq!(&bytes[..4], b"FXDU");
    assert_eq!(Request::decode(&bytes), Ok(request));
    assert_eq!(request.name(), b"gpu");
}

#[test]
fn a_name_must_fit_and_be_one() {
    assert_eq!(Request::new(b"", ANY, 1), None);
    assert_eq!(Request::new(&[b'a'; 32], ANY, 1), None);
    assert_eq!(Request::new(b"g\0u", ANY, 1), None);
    assert!(Request::new(&[b'a'; 31], ANY, 1).is_some());
}

#[test]
fn a_request_is_refused_for_what_is_wrong() {
    let good = Request::new(b"gpu", 0x10, 4096).expect("fits").encode();
    assert_eq!(Request::decode(&good[..47]), Err(Refused::Length));
    let mut magic = good;
    magic[0] = b'X';
    assert_eq!(Request::decode(&magic), Err(Refused::Magic));
    let mut empty = good;
    empty[16] = 0;
    assert_eq!(Request::decode(&empty), Err(Refused::Name));
    let mut after_nul = good;
    after_nul[40] = b'x';
    assert_eq!(Request::decode(&after_nul), Err(Refused::Name));
    let zero = Request::new(b"gpu", 0x10, 0).expect("fits").encode();
    assert_eq!(Request::decode(&zero), Err(Refused::ImageLength));
    let large = Request::new(b"gpu", 0x10, MAX_IMAGE + 1)
        .expect("fits")
        .encode();
    assert_eq!(Request::decode(&large), Err(Refused::ImageLength));
    let largest = Request::new(b"gpu", 0x10, MAX_IMAGE)
        .expect("fits")
        .encode();
    assert!(Request::decode(&largest).is_ok());
}

#[test]
fn every_answer_round_trips() {
    for value in 0..10 {
        let outcome = Outcome::from_u32(value).expect("every number below ten");
        assert_eq!(outcome as u32, value);
        let answer = Answer {
            outcome,
            updated: 1,
            tried: 2,
        };
        let bytes = answer.encode();
        assert_eq!(bytes.len(), ANSWER_BYTES);
        assert_eq!(Answer::decode(&bytes), Ok(answer));
    }
    assert_eq!(Outcome::from_u32(10), None);
    let mut unknown = Answer::of(Outcome::Updated).encode();
    unknown[4] = 10;
    assert_eq!(Answer::decode(&unknown), Err(Refused::Outcome));
    let mut magic = Answer::of(Outcome::Updated).encode();
    magic[3] = b'U';
    assert_eq!(Answer::decode(&magic), Err(Refused::Magic));
}

#[test]
fn the_fingerprint_is_fnv1a_64() {
    // The published FNV-1a 64 test vectors.
    assert_eq!(Fingerprint::new().value(), 0xcbf2_9ce4_8422_2325);
    let mut a = Fingerprint::new();
    a.update(b"a");
    assert_eq!(a.value(), 0xaf63_dc4c_8601_ec8c);
    let mut foobar = Fingerprint::new();
    foobar.update(b"foo");
    foobar.update(b"bar");
    assert_eq!(foobar.value(), 0x8594_4171_f739_67e8);
}
