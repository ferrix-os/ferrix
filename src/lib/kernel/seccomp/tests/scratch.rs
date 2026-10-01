//! The verifier's rule 7 against the real kernel's answers.
//!
//! `data/scratch-<n>.bpf` are small programs that read a scratch word after a
//! `RET`, a jump or a store on one path only; `data/scratch-<n>.answer` is what
//! a Linux 7.0 host's seccomp said of each (`oracle.c --accept`, run on
//! nazuna): `accepted`, or `refused 22`. `verify` and the naive checker must
//! give the same answer, and so must Linux's. S1 passed program 1 and
//! program 6, which Linux refuses, because it carried nothing from a `RET` to
//! the next instruction (the certification consultant found it).

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a test that fails by panicking"
)]

mod naive;

use ferrix_seccomp::{Insn, verify};

const PROGRAMS: [(&[u8], &str); 6] = [
    (
        include_bytes!("data/scratch-1.bpf"),
        include_str!("data/scratch-1.answer"),
    ),
    (
        include_bytes!("data/scratch-2.bpf"),
        include_str!("data/scratch-2.answer"),
    ),
    (
        include_bytes!("data/scratch-3.bpf"),
        include_str!("data/scratch-3.answer"),
    ),
    (
        include_bytes!("data/scratch-4.bpf"),
        include_str!("data/scratch-4.answer"),
    ),
    (
        include_bytes!("data/scratch-5.bpf"),
        include_str!("data/scratch-5.answer"),
    ),
    (
        include_bytes!("data/scratch-6.bpf"),
        include_str!("data/scratch-6.answer"),
    ),
];

#[test]
fn verify_and_the_naive_checker_answer_as_linux_did() {
    for (index, (bytes, answer)) in PROGRAMS.iter().enumerate() {
        let insns: Vec<Insn> = bytes
            .chunks_exact(8)
            .map(|chunk| Insn::from_bytes(chunk).expect("eight bytes"))
            .collect();
        let linux_accepts = answer.trim() == "accepted";
        assert!(
            linux_accepts || answer.trim() == "refused 22",
            "scratch-{}: an answer oracle.c does not print",
            index + 1
        );
        assert_eq!(
            verify(&insns).is_ok(),
            linux_accepts,
            "scratch-{}: verify and Linux differ ({})",
            index + 1,
            answer.trim()
        );
        assert_eq!(
            naive::naive(&insns),
            linux_accepts,
            "scratch-{}: the naive checker and Linux differ ({})",
            index + 1,
            answer.trim()
        );
    }
}
