//! Checking a program before it is installed: Linux's `bpf_check_classic`
//! followed by `seccomp_check_filter`, rule for rule (`docs/SECCOMP.md`
//! §3.1). Each rule names the line of Linux's check it mirrors.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::{DATA_BYTES, Insn, MAX_INSNS, MEMWORDS};

// Instruction classes, sizes, modes, operators and sources, from
// `linux/bpf_common.h`.
const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;

const CLASS: u16 = 0x07;
const SIZE_W: u16 = 0x00;
const MODE_IMM: u16 = 0x00;
const MODE_ABS: u16 = 0x20;
const MODE_MEM: u16 = 0x60;
const MODE_LEN: u16 = 0x80;
const SRC_X: u16 = 0x08;
const OP: u16 = 0xf0;

const ADD: u16 = 0x00;
const SUB: u16 = 0x10;
const MUL: u16 = 0x20;
const DIV: u16 = 0x30;
const OR: u16 = 0x40;
const AND: u16 = 0x50;
const LSH: u16 = 0x60;
const RSH: u16 = 0x70;
const NEG: u16 = 0x80;
const XOR: u16 = 0xa0;

const JA: u16 = 0x00;
const JEQ: u16 = 0x10;
const JGT: u16 = 0x20;
const JGE: u16 = 0x30;
const JSET: u16 = 0x40;

const RVAL_A: u16 = 0x10;
const TAX: u16 = 0x00;
const TXA: u16 = 0x80;

/// An arithmetic operator.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Or,
    And,
    Lsh,
    Rsh,
    Xor,
}

impl Arith {
    /// `a op b`, or `None` for a division by zero.
    pub(crate) fn apply(self, a: u32, b: u32) -> Option<u32> {
        Some(match self {
            Arith::Add => a.wrapping_add(b),
            Arith::Sub => a.wrapping_sub(b),
            Arith::Mul => a.wrapping_mul(b),
            Arith::Div => a.checked_div(b)?,
            Arith::Or => a | b,
            Arith::And => a & b,
            Arith::Lsh => a << (b & 31),
            Arith::Rsh => a >> (b & 31),
            Arith::Xor => a ^ b,
        })
    }
}

/// A conditional jump's test.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Test {
    Eq,
    Gt,
    Ge,
    Set,
}

impl Test {
    /// Whether the test holds for `a` against `b`.
    pub(crate) fn holds(self, a: u32, b: u32) -> bool {
        match self {
            Test::Eq => a == b,
            Test::Gt => a > b,
            Test::Ge => a >= b,
            Test::Set => a & b != 0,
        }
    }
}

/// An instruction the machine runs, decoded from its opcode. [`decode`] is the
/// one place an opcode is read, and [`verify`] lets through exactly the codes
/// it decodes to something other than [`Op::Invalid`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Op {
    LoadAbs,
    LoadLen,
    LoadImm,
    LoadMem,
    LoadXLen,
    LoadXImm,
    LoadXMem,
    Store,
    StoreX,
    Alu(Arith, bool),
    Neg,
    Jump,
    Branch(Test, bool),
    ReturnK,
    ReturnA,
    Tax,
    Txa,
    Invalid,
}

/// The operation an opcode is, or [`Op::Invalid`] for any the seccomp filter
/// of Linux does not allow (`seccomp_check_filter`'s list): everything but the
/// ones the rules of [`verify`] name.
pub(crate) fn decode(code: u16) -> Op {
    let from_x = code & SRC_X != 0;
    match code & CLASS {
        LD => match code & !CLASS {
            c if c == SIZE_W | MODE_ABS => Op::LoadAbs,
            c if c == SIZE_W | MODE_LEN => Op::LoadLen,
            MODE_IMM => Op::LoadImm,
            MODE_MEM => Op::LoadMem,
            _ => Op::Invalid,
        },
        LDX => match code & !CLASS {
            c if c == SIZE_W | MODE_LEN => Op::LoadXLen,
            MODE_IMM => Op::LoadXImm,
            MODE_MEM => Op::LoadXMem,
            _ => Op::Invalid,
        },
        ST if code == ST => Op::Store,
        STX if code == STX => Op::StoreX,
        ALU if code & !(CLASS | OP | SRC_X) == 0 => match code & OP {
            ADD => Op::Alu(Arith::Add, from_x),
            SUB => Op::Alu(Arith::Sub, from_x),
            MUL => Op::Alu(Arith::Mul, from_x),
            DIV => Op::Alu(Arith::Div, from_x),
            OR => Op::Alu(Arith::Or, from_x),
            AND => Op::Alu(Arith::And, from_x),
            LSH => Op::Alu(Arith::Lsh, from_x),
            RSH => Op::Alu(Arith::Rsh, from_x),
            XOR => Op::Alu(Arith::Xor, from_x),
            NEG if !from_x => Op::Neg,
            _ => Op::Invalid,
        },
        JMP if code & !(CLASS | OP | SRC_X) == 0 => match code & OP {
            JA if !from_x => Op::Jump,
            JEQ => Op::Branch(Test::Eq, from_x),
            JGT => Op::Branch(Test::Gt, from_x),
            JGE => Op::Branch(Test::Ge, from_x),
            JSET => Op::Branch(Test::Set, from_x),
            _ => Op::Invalid,
        },
        RET => match code & !CLASS {
            0 => Op::ReturnK,
            RVAL_A => Op::ReturnA,
            _ => Op::Invalid,
        },
        MISC => match code & !CLASS {
            TAX => Op::Tax,
            TXA => Op::Txa,
            _ => Op::Invalid,
        },
        _ => Op::Invalid,
    }
}

/// Why a filter was refused, with the index of the instruction at fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// No instructions, or more than [`MAX_INSNS`].
    Length,
    /// An opcode a seccomp filter may not use.
    Opcode(usize),
    /// A `LD|W|ABS` outside `seccomp_data` or not word-aligned.
    Offset(usize),
    /// A jump past the end of the program.
    Jump(usize),
    /// A scratch index of [`MEMWORDS`] or more.
    Memory(usize),
    /// A division or modulus by the constant zero, or a constant shift of 32
    /// or more.
    Arithmetic(usize),
    /// A scratch word read where some path reaches it without a store.
    Uninitialised(usize),
    /// The last instruction is not a return.
    NoReturn,
    /// No memory for the verified copy.
    NoMemory,
}

/// A program that passed [`verify`]: the only way to hold one, so the machine
/// may rely on every rule having held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program(Box<[Insn]>);

impl Program {
    /// Its instructions.
    #[must_use]
    pub fn insns(&self) -> &[Insn] {
        &self.0
    }

    /// How many instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it has none, which a verified one never does.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Check `raw` as Linux does, and answer the program it is.
///
/// 1. The length is 1 to [`MAX_INSNS`] (`bpf_check_classic`'s `flen`).
/// 2. Only the opcodes of `seccomp_check_filter`'s list: `RET K/A`; `ALU`
///    `ADD SUB MUL DIV AND OR XOR LSH RSH` against `K` or `X`, and `NEG`;
///    `LD/LDX IMM`, `LD/LDX MEM`, `ST`, `STX`; `TAX`, `TXA`; `LD|W|ABS`;
///    `LD|W|LEN`, `LDX|W|LEN`; `JA` and `JEQ JGT JGE JSET` against `K` or
///    `X`. Everything else -- `MOD`, byte and half-word loads, `IND`, `MSH`
///    -- is refused.
/// 3. `LD|W|ABS` is word-aligned and inside `seccomp_data`.
/// 4. A constant `DIV` by zero and a constant shift of 32 or more are refused
///    (`bpf_check_classic`); a scratch index is below [`MEMWORDS`].
/// 5. Jumps stay forward and inside the program: `JA` needs
///    `k < len - pc - 1`, a conditional jump `pc + jt + 1 < len` and
///    `pc + jf + 1 < len`.
/// 6. The last instruction is a return.
/// 7. No scratch word is read on any path before a store to it
///    (`check_load_and_stores`; CVE-2010-4158).
///
/// # Errors
///
/// [`Invalid`], naming the first instruction at fault, in this order of
/// rules.
pub fn verify(raw: &[Insn]) -> Result<Program, Invalid> {
    if raw.is_empty() || raw.len() > MAX_INSNS {
        return Err(Invalid::Length);
    }
    for (index, insn) in raw.iter().enumerate() {
        check_insn(insn, index, raw.len() - index - 1)?;
    }
    if !raw
        .last()
        .is_some_and(|last| matches!(decode(last.code), Op::ReturnK | Op::ReturnA))
    {
        return Err(Invalid::NoReturn);
    }
    check_scratch(raw)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(raw.len())
        .map_err(|_| Invalid::NoMemory)?;
    copy.extend_from_slice(raw);
    Ok(Program(copy.into_boxed_slice()))
}

/// Rules 2 to 5 for one instruction; `after` is how many follow it.
fn check_insn(insn: &Insn, index: usize, after: usize) -> Result<(), Invalid> {
    let constant = insn.code & SRC_X == 0;
    match decode(insn.code) {
        Op::Invalid => Err(Invalid::Opcode(index)),
        Op::LoadAbs if insn.k as usize >= DATA_BYTES || !insn.k.is_multiple_of(4) => {
            Err(Invalid::Offset(index))
        }
        Op::LoadMem | Op::LoadXMem | Op::Store | Op::StoreX if insn.k as usize >= MEMWORDS => {
            Err(Invalid::Memory(index))
        }
        Op::Alu(Arith::Div, _) if constant && insn.k == 0 => Err(Invalid::Arithmetic(index)),
        Op::Alu(Arith::Lsh | Arith::Rsh, _) if constant && insn.k >= 32 => {
            Err(Invalid::Arithmetic(index))
        }
        Op::Jump if insn.k as usize >= after => Err(Invalid::Jump(index)),
        Op::Branch(..) if usize::from(insn.jt) >= after || usize::from(insn.jf) >= after => {
            Err(Invalid::Jump(index))
        }
        _ => Ok(()),
    }
}

/// Rule 7, as Linux's `check_load_and_stores` does it, line for line.
///
/// One running set of the scratch words stored (`valid`), and a mask of what
/// every jump to an instruction carried (`masks`, all ones until a jump
/// narrows it). At each instruction the running set is narrowed by that
/// instruction's mask; a store adds its word; a conditional jump narrows the
/// masks of both its targets by the running set; an unconditional jump narrows
/// its target's and then makes the running set everything, because nothing
/// falls through it. A `RET` changes nothing: the instruction after it takes
/// what ran into the `RET`, and a read there is judged by that. So
/// `0 JEQ jt=2; 1 ST M[0]; 2 JA 1; 3 RET; 4 LD M[0]; 5 RET A` is refused: the
/// `LD` is reached by the jump, which stored, and by the fall through the
/// `RET`, which did not.
fn check_scratch(raw: &[Insn]) -> Result<(), Invalid> {
    let mut masks: Vec<u16> = Vec::new();
    masks
        .try_reserve_exact(raw.len())
        .map_err(|_| Invalid::NoMemory)?;
    masks.resize(raw.len(), u16::MAX);
    let mut valid = 0_u16;
    for (index, insn) in raw.iter().enumerate() {
        valid &= masks.get(index).copied().unwrap_or(u16::MAX);
        let word = 1_u16 << (insn.k % MEMWORDS as u32);
        let mut narrow = |to: usize, by: u16| {
            if let Some(mask) = masks.get_mut(to) {
                *mask &= by;
            }
        };
        match decode(insn.code) {
            Op::Store | Op::StoreX => valid |= word,
            Op::LoadMem | Op::LoadXMem if valid & word == 0 => {
                return Err(Invalid::Uninitialised(index));
            }
            Op::Jump => {
                narrow(index + 1 + insn.k as usize, valid);
                valid = u16::MAX;
            }
            Op::Branch(..) => {
                narrow(index + 1 + usize::from(insn.jt), valid);
                narrow(index + 1 + usize::from(insn.jf), valid);
            }
            _ => {}
        }
    }
    Ok(())
}
