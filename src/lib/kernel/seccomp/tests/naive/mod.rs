//! A second checker for seccomp filters, written from the rules by a different
//! method than `verify`: the opcodes spelled out as a list where the verifier
//! decodes them, and the scratch words as the edges into each instruction where
//! the verifier carries one running set (`docs/SECCOMP.md` §3.1). The crate's agreement test
//! and the fuzz target `seccomp_verify_run` both hold `verify` to it.

#![allow(
    clippy::indexing_slicing,
    clippy::manual_is_multiple_of,
    reason = "a checker written to be obviously the rules"
)]

use ferrix_seccomp::{DATA_BYTES, Insn, MAX_INSNS, MEMWORDS};

/// Every opcode `seccomp_check_filter` lets through, spelled out.
pub(crate) fn allowed(code: u16) -> bool {
    const ALU_OPS: [u16; 9] = [0x00, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0xa0];
    const JUMPS: [u16; 4] = [0x10, 0x20, 0x30, 0x40];
    match code {
        // LD: ABS, LEN, IMM, MEM. LDX: LEN, IMM, MEM. ST, STX.
        0x20 | 0x80 | 0x00 | 0x60 | 0x81 | 0x01 | 0x61 | 0x02 | 0x03 => true,
        // NEG, TAX, TXA, JA, RET K, RET A.
        0x84 | 0x07 | 0x87 | 0x05 | 0x06 | 0x16 => true,
        other => {
            let class = other & 0x07;
            let source = other & 0x08;
            let op = other & 0xf0;
            (class == 0x04 && ALU_OPS.contains(&op) && other & !0xfc == 0 && source <= 0x08)
                || (class == 0x05 && JUMPS.contains(&op) && other & !0x7d == 0)
        }
    }
}

/// The rules, checked by a second method.
pub(crate) fn naive(raw: &[Insn]) -> bool {
    if raw.is_empty() || raw.len() > MAX_INSNS {
        return false;
    }
    let len = raw.len();
    for (i, insn) in raw.iter().enumerate() {
        if !allowed(insn.code) {
            return false;
        }
        match insn.code {
            0x20 if insn.k >= DATA_BYTES as u32 || insn.k % 4 != 0 => return false,
            0x34 if insn.k == 0 => return false,
            0x64 | 0x74 if insn.k >= 32 => return false,
            0x60 | 0x61 | 0x02 | 0x03 if insn.k >= MEMWORDS as u32 => return false,
            0x05 if insn.k as usize >= len - i - 1 => return false,
            _ => {}
        }
        if matches!(insn.code & 0xf7, 0x15 | 0x25 | 0x35 | 0x45)
            && (i + 1 + usize::from(insn.jt) >= len || i + 1 + usize::from(insn.jf) >= len)
        {
            return false;
        }
    }
    if !matches!(raw.last().map(|insn| insn.code), Some(0x06 | 0x16)) {
        return false;
    }
    // Linux's rule for scratch words, written as the edges into each
    // instruction: what reaches instruction `i` is the set stored before the
    // instruction ahead of it, plus its own store -- or everything, if that was
    // an unconditional jump, which nothing falls through -- and, for each jump
    // to `i`, the set stored before that jump. A `RET` is not special: the
    // instruction after it takes what ran into it. Jumps only go forward, so
    // one pass fills `jumped_in` before it is read.
    let mut jumped_in: Vec<Vec<u16>> = vec![vec![]; len];
    let mut before = vec![0_u16; len];
    for (i, insn) in raw.iter().enumerate() {
        let falling = if i == 0 {
            0
        } else if raw[i - 1].code == 0x05 {
            u16::MAX
        } else if matches!(raw[i - 1].code, 0x02 | 0x03) {
            before[i - 1] | (1 << (raw[i - 1].k % 16))
        } else {
            before[i - 1]
        };
        before[i] = jumped_in[i].iter().fold(falling, |all, jump| all & jump);
        if matches!(insn.code, 0x60 | 0x61) && before[i] & (1 << insn.k) == 0 {
            return false;
        }
        match insn.code {
            0x05 => jumped_in[i + 1 + insn.k as usize].push(before[i]),
            c if matches!(c & 0xf7, 0x15 | 0x25 | 0x35 | 0x45) => {
                jumped_in[i + 1 + usize::from(insn.jt)].push(before[i]);
                jumped_in[i + 1 + usize::from(insn.jf)].push(before[i]);
            }
            _ => {}
        }
    }
    true
}
