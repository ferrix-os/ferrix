//! Chromium's own filters, run against the answers the real kernel gave.
//!
//! `data/chrome-<n>.bpf` are the three distinct programs nine of Chrome 151's
//! processes installed (`extract.py`, from an `strace -f -v` on a Linux 7.0
//! host), `data/chrome-<n>.verdicts` what that kernel did with each call
//! number 0 to 449 under them (`oracle.c`). Each program must pass `verify`, and
//! `run` must give, for every call, the action the kernel did.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "a test that fails by panicking"
)]

use ferrix_seccomp::{ACTION_FULL, ALLOW, DATA, ERRNO, Insn, SeccompData, TRAP, run, verify};

const X86_64: u32 = 0xc000_003e;
const I386: u32 = 0x4000_0003;

const PROGRAMS: [(&[u8], &str); 3] = [
    (
        include_bytes!("data/chrome-1.bpf"),
        include_str!("data/chrome-1.verdicts"),
    ),
    (
        include_bytes!("data/chrome-2.bpf"),
        include_str!("data/chrome-2.verdicts"),
    ),
    (
        include_bytes!("data/chrome-3.bpf"),
        include_str!("data/chrome-3.verdicts"),
    ),
];

fn load(bytes: &[u8]) -> ferrix_seccomp::Program {
    let insns: Vec<Insn> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| Insn::from_bytes(chunk).expect("eight bytes"))
        .collect();
    verify(&insns).expect("a filter the real kernel accepted")
}

fn call(nr: i32, arch: u32) -> SeccompData {
    SeccompData {
        nr,
        arch,
        instruction_pointer: 0,
        args: [0; 6],
    }
}

#[test]
fn chromes_filters_pass_the_verifier() {
    for (bytes, _) in PROGRAMS {
        let program = load(bytes);
        assert!((600..=700).contains(&program.len()));
    }
}

#[test]
fn every_call_gets_the_answer_the_kernel_gave() {
    let mut differences = Vec::new();
    for (index, (bytes, verdicts)) in PROGRAMS.into_iter().enumerate() {
        let program = load(bytes);
        let mut checked = 0;
        for line in verdicts.lines() {
            let mut fields = line.split_whitespace();
            let nr: i32 = fields.next().unwrap().parse().unwrap();
            let kind = fields.next().unwrap();
            let data: u32 = fields.next().map_or(0, |value| value.parse().unwrap());
            let got = run(&program, &call(nr, X86_64));
            // x86-64 `uretprobe` (335) and `uprobe` (336): Linux passes them
            // through seccomp unfiltered (`__secure_computing`), because
            // their kernel-made callers could not be told what a filter
            // said. Ferrix has no probes, so a filter judges them like any
            // number it does not know -- here, a trap -- which is stricter
            // and safe, and the one place the oracle and this differ.
            let passes_through = matches!(nr, 335 | 336);
            let agrees = passes_through && kind == "allow"
                || match kind {
                    "trap" => got == TRAP | data,
                    "errno" => got == ERRNO | data,
                    // The call ran -- or failed with the errno it fails with
                    // anyway, which a filter's ERRNO cannot be told from.
                    "allow" => got == ALLOW || got & ACTION_FULL == ERRNO,
                    other => panic!("the oracle said {other}"),
                };
            if !agrees {
                differences.push(format!(
                    "program {index}, call {nr}: kernel `{line}`, here {got:#x}"
                ));
            }
            checked += 1;
        }
        assert_eq!(checked, 450);
    }
    assert!(
        differences.is_empty(),
        "{}",
        differences.join(
            "
"
        )
    );
}

#[test]
fn another_architecture_and_the_x32_bit_are_trapped() {
    // What the decoded programs begin with: `ld [4]; jeq AUDIT_ARCH_X86_64;
    // ret TRAP|10`, then `ld [0]; jset 0x40000000; ret TRAP|9`.
    for (bytes, _) in PROGRAMS {
        let program = load(bytes);
        assert_eq!(run(&program, &call(0, I386)), TRAP | 10);
        assert_eq!(run(&program, &call(0x4000_0000, X86_64)), TRAP | 9);
    }
}

#[test]
fn a_trap_carries_its_number_in_the_data_half() {
    for (bytes, _) in PROGRAMS {
        let program = load(bytes);
        let got = run(&program, &call(0, I386));
        assert_eq!(got & ACTION_FULL, TRAP);
        assert_eq!(got & DATA, 10);
    }
}
