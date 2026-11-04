//! Fuzz the file protocol's decoders, which `btrfsd` runs on what a Linux partition's
//! `linuxd` sends (`docs/microkernel/BTRFSD.md` §3, L.object.330, BD.9).
//!
//! Beyond not panicking: what decodes encodes back to what decodes the same, and a name
//! that decodes is a good name.

#![no_main]

use ferrix_fsproto::wire::{Answer, DiskUp, REQUEST_MAX, Request, decode_dirent, name_is_good};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(request) = Request::decode(bytes) {
        let mut out = [0_u8; REQUEST_MAX];
        let length = request.encode(&mut out).expect("what decoded fits");
        let again = Request::decode(&out[..length]).expect("what was encoded decodes");
        assert_eq!(format!("{request:?}"), format!("{again:?}"));
        if !request.name.is_empty() {
            assert!(name_is_good(request.name));
        }
    }
    if let Some(answer) = Answer::decode(bytes) {
        assert!(Answer::decode(&answer.encode()).is_some());
    }
    if let Some(up) = DiskUp::decode(bytes) {
        assert!(DiskUp::decode(&up.encode()).is_some());
    }
    if let Some((_, _, _, name, length)) = decode_dirent(bytes) {
        assert!(length <= bytes.len());
        assert!(name.len() <= length);
    }
});
