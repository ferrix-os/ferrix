//! The classic machine: `A`, `X`, sixteen scratch words, a program counter
//! that only moves forward.

use crate::verify::{Op, Program, decode};
use crate::{
    ALLOW, DATA_BYTES, KILL_PROCESS, KILL_THREAD, MEMWORDS, SeccompData, more_restrictive,
};

/// Run a verified `program` against `data`, and answer what it returned.
///
/// `A`, `X` and the scratch words start at zero. Arithmetic is 32-bit and
/// wraps; a shift by `X` is masked to five bits, as Linux's interpreter does
/// (`kernel/bpf/core.c`'s `SHT`); a division by an `X` of zero ends the
/// program returning 0, which is [`KILL_THREAD`], as Linux's conversion to
/// its internal BPF emits for `DIV X`.
///
/// Forward jumps only: a program of `n` instructions stops within `n` steps,
/// and the loop is bounded by `n` as well, so a program that somehow ran off
/// its end would be answered [`KILL_THREAD`] and not run on.
#[must_use]
pub fn run(program: &Program, data: &SeccompData) -> u32 {
    run_counted(program, data).0
}

/// [`run`], and how many instructions it took: what the fuzzer holds to the
/// program's length.
#[must_use]
pub fn run_counted(program: &Program, data: &SeccompData) -> (u32, usize) {
    let insns = program.insns();
    let (mut a, mut x) = (0_u32, 0_u32);
    let mut scratch = [0_u32; MEMWORDS];
    let mut pc = 0_usize;
    for steps in 1..=insns.len() + 1 {
        let Some(&insn) = insns.get(pc) else {
            return (KILL_THREAD, steps - 1);
        };
        pc += 1;
        let source = |from_x: bool| if from_x { x } else { insn.k };
        match decode(insn.code) {
            Op::LoadAbs => a = data.word(insn.k).unwrap_or(0),
            Op::LoadLen => a = DATA_BYTES as u32,
            Op::LoadImm => a = insn.k,
            Op::LoadMem => a = scratch.get(insn.k as usize).copied().unwrap_or(0),
            Op::LoadXLen => x = DATA_BYTES as u32,
            Op::LoadXImm => x = insn.k,
            Op::LoadXMem => x = scratch.get(insn.k as usize).copied().unwrap_or(0),
            Op::Store => {
                if let Some(slot) = scratch.get_mut(insn.k as usize) {
                    *slot = a;
                }
            }
            Op::StoreX => {
                if let Some(slot) = scratch.get_mut(insn.k as usize) {
                    *slot = x;
                }
            }
            Op::Alu(op, from_x) => match op.apply(a, source(from_x)) {
                Some(result) => a = result,
                None => return (KILL_THREAD, steps),
            },
            Op::Neg => a = a.wrapping_neg(),
            Op::Jump => pc = pc.saturating_add(insn.k as usize),
            Op::Branch(test, from_x) => {
                let taken = test.holds(a, source(from_x));
                pc += usize::from(if taken { insn.jt } else { insn.jf });
            }
            Op::ReturnK => return (insn.k, steps),
            Op::ReturnA => return (a, steps),
            Op::Tax => x = a,
            Op::Txa => a = x,
            Op::Invalid => return (KILL_THREAD, steps),
        }
    }
    (KILL_THREAD, insns.len())
}

/// Run every filter, newest first, and answer the most restrictive result. On
/// a tie the newer one's data stands, as Linux's loop keeps the first it
/// found. No filters at all is [`KILL_PROCESS`]: a thread in filter mode has at
/// least one, so an empty chain is a defect of the caller's and not a license,
/// as Linux's `seccomp_run_filters` answers it (`WARN_ON(f == NULL)`).
#[must_use]
pub fn run_all<'a>(filters: impl IntoIterator<Item = &'a Program>, data: &SeccompData) -> u32 {
    let mut filters = filters.into_iter().peekable();
    if filters.peek().is_none() {
        return KILL_PROCESS;
    }
    let mut answer = ALLOW;
    for program in filters {
        let result = run(program, data);
        if more_restrictive(result, answer) {
            answer = result;
        }
    }
    answer
}
