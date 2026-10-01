//! What `verify` refuses, one test per rule (each citing the line of Linux's
//! check it mirrors), and what `run` answers, one per opcode.

extern crate std;

use std::vec;
use std::vec::Vec;

use super::*;

// Opcodes, spelled as `linux/bpf_common.h` spells them.
const LD_W_ABS: u16 = 0x20;
const LD_W_LEN: u16 = 0x80;
const LD_IMM: u16 = 0x00;
const LD_MEM: u16 = 0x60;
const LDX_W_LEN: u16 = 0x81;
const LDX_IMM: u16 = 0x01;
const LDX_MEM: u16 = 0x61;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU_K: u16 = 0x04;
const ALU_X: u16 = 0x0c;
const ADD: u16 = 0x00;
const SUB: u16 = 0x10;
const MUL: u16 = 0x20;
const DIV: u16 = 0x30;
const OR: u16 = 0x40;
const AND: u16 = 0x50;
const LSH: u16 = 0x60;
const RSH: u16 = 0x70;
const NEG: u16 = 0x80;
const MOD: u16 = 0x90;
const XOR: u16 = 0xa0;
const JA: u16 = 0x05;
const JEQ_K: u16 = 0x15;
const JGT_K: u16 = 0x25;
const JGE_K: u16 = 0x35;
const JSET_K: u16 = 0x45;
const JEQ_X: u16 = 0x1d;
const RET_K: u16 = 0x06;
const RET_A: u16 = 0x16;
const TAX: u16 = 0x07;
const TXA: u16 = 0x87;

fn ins(code: u16, k: u32) -> Insn {
    Insn::new(code, 0, 0, k)
}

fn ret(k: u32) -> Insn {
    ins(RET_K, k)
}

fn call(nr: i32) -> SeccompData {
    SeccompData {
        nr,
        arch: 0xc000_003e,
        instruction_pointer: 0x1122_3344_5566_7788,
        args: [1, 2, 3, 4, 5, u64::MAX],
    }
}

fn verified(program: &[Insn]) -> Program {
    verify(program).unwrap_or_else(|why| panic!("refused: {why:?}"))
}

/// Run `body` then return A.
fn answer(body: &[Insn]) -> u32 {
    let mut program: Vec<Insn> = body.to_vec();
    program.push(ins(RET_A, 0));
    run(&verified(&program), &call(1))
}

// ---- verify: the rules ----------------------------------------------------

#[test]
fn rule_1_the_length_is_one_to_4096() {
    assert_eq!(verify(&[]), Err(Invalid::Length));
    assert!(verify(&vec![ret(ALLOW); MAX_INSNS]).is_ok());
    assert_eq!(
        verify(&vec![ret(ALLOW); MAX_INSNS + 1]),
        Err(Invalid::Length)
    );
    assert!(verify(&[ret(ALLOW)]).is_ok());
}

#[test]
fn rule_2_every_opcode_seccomp_allows_is_accepted() {
    let allowed = [
        ins(LD_W_ABS, 0),
        ins(LD_W_LEN, 0),
        ins(LD_IMM, 7),
        ins(LDX_W_LEN, 0),
        ins(LDX_IMM, 7),
        ins(ST, 0),
        ins(STX, 1),
        ins(LD_MEM, 0),
        ins(LDX_MEM, 1),
        ins(ALU_K | ADD, 1),
        ins(ALU_K | SUB, 1),
        ins(ALU_K | MUL, 1),
        ins(ALU_K | DIV, 1),
        ins(ALU_K | OR, 1),
        ins(ALU_K | AND, 1),
        ins(ALU_K | LSH, 1),
        ins(ALU_K | RSH, 1),
        ins(ALU_K | XOR, 1),
        ins(ALU_X | ADD, 0),
        ins(ALU_X | SUB, 0),
        ins(ALU_X | MUL, 0),
        ins(ALU_X | DIV, 0),
        ins(ALU_X | OR, 0),
        ins(ALU_X | AND, 0),
        ins(ALU_X | LSH, 0),
        ins(ALU_X | RSH, 0),
        ins(ALU_X | XOR, 0),
        ins(ALU_K | NEG, 0),
        ins(TAX, 0),
        ins(TXA, 0),
        Insn::new(JEQ_K, 0, 0, 1),
        Insn::new(JGT_K, 0, 0, 1),
        Insn::new(JGE_K, 0, 0, 1),
        Insn::new(JSET_K, 0, 0, 1),
        Insn::new(JEQ_X, 0, 0, 0),
        ins(JA, 0),
    ];
    for insn in allowed {
        // A store first so a scratch read is allowed, and a return last.
        let program = [ins(ST, 0), ins(STX, 1), insn, ret(ALLOW)];
        assert!(verify(&program).is_ok(), "{insn:?} was refused");
    }
    assert!(verify(&[ret(ALLOW)]).is_ok());
    assert!(verify(&[ins(RET_A, 0)]).is_ok());
}

#[test]
fn rule_2_everything_else_is_refused() {
    let refused = [
        // MOD, and the half-word and byte loads.
        ins(ALU_K | MOD, 3),
        ins(ALU_X | MOD, 0),
        ins(0x28, 0),
        ins(0x30, 0),
        // Indexed loads and MSH.
        ins(0x40, 0),
        ins(0x48, 0),
        ins(0x50, 0),
        ins(0xb1, 0),
        // LDX cannot load from the data.
        ins(0x21, 0),
        // Extra bits on stores, returns, NEG against X, JA against X.
        ins(ST | 0x20, 0),
        ins(STX | 0x08, 0),
        ins(0x26, 0),
        ins(0x36, 0),
        ins(ALU_X | NEG, 0),
        ins(0x0d, 0),
        // MISC other than TAX and TXA.
        ins(0x17, 0),
        ins(0x47, 0),
        // A jump operator that is not one.
        ins(0x55, 0),
        ins(0x65, 0),
        // A class bit pattern nobody defined.
        ins(0xffff, 0),
    ];
    for insn in refused {
        let program = [insn, ret(ALLOW)];
        assert_eq!(verify(&program), Err(Invalid::Opcode(0)), "{insn:?}");
    }
}

#[test]
fn rule_3_an_absolute_load_is_aligned_and_inside_the_data() {
    for k in [0, 4, 60] {
        assert!(verify(&[ins(LD_W_ABS, k), ret(ALLOW)]).is_ok(), "{k}");
    }
    for k in [1, 2, 3, 62, 64, 68, u32::MAX] {
        assert_eq!(
            verify(&[ins(LD_W_ABS, k), ret(ALLOW)]),
            Err(Invalid::Offset(0)),
            "{k}"
        );
    }
}

#[test]
fn rule_4_a_constant_division_by_zero_and_a_shift_of_32_are_refused() {
    assert_eq!(
        verify(&[ins(ALU_K | DIV, 0), ret(ALLOW)]),
        Err(Invalid::Arithmetic(0))
    );
    // Against X it is the machine's business, not the verifier's.
    assert!(verify(&[ins(ALU_X | DIV, 0), ret(ALLOW)]).is_ok());
    for op in [LSH, RSH] {
        assert!(verify(&[ins(ALU_K | op, 31), ret(ALLOW)]).is_ok());
        assert_eq!(
            verify(&[ins(ALU_K | op, 32), ret(ALLOW)]),
            Err(Invalid::Arithmetic(0))
        );
        assert!(verify(&[ins(ALU_X | op, 99), ret(ALLOW)]).is_ok());
    }
}

#[test]
fn rule_4_a_scratch_index_is_below_16() {
    for code in [ST, STX] {
        assert!(verify(&[ins(code, 15), ret(ALLOW)]).is_ok());
        assert_eq!(
            verify(&[ins(code, 16), ret(ALLOW)]),
            Err(Invalid::Memory(0))
        );
    }
    for code in [LD_MEM, LDX_MEM] {
        assert_eq!(
            verify(&[ins(code, 16), ret(ALLOW)]),
            Err(Invalid::Memory(0))
        );
    }
}

#[test]
fn rule_5_jumps_go_forward_and_stay_inside() {
    // `JA` needs `k < len - pc - 1`: with one instruction after it, only 0.
    assert!(verify(&[ins(JA, 0), ret(ALLOW)]).is_ok());
    assert_eq!(verify(&[ins(JA, 1), ret(ALLOW)]), Err(Invalid::Jump(0)));
    assert!(verify(&[ins(JA, 1), ret(ALLOW), ret(ALLOW)]).is_ok());
    // A conditional jump's two targets, each on its own.
    let after = ret(ALLOW);
    assert!(verify(&[Insn::new(JEQ_K, 1, 0, 0), after, after]).is_ok());
    assert_eq!(
        verify(&[Insn::new(JEQ_K, 2, 0, 0), after, after]),
        Err(Invalid::Jump(0))
    );
    assert_eq!(
        verify(&[Insn::new(JEQ_K, 0, 2, 0), after, after]),
        Err(Invalid::Jump(0))
    );
    // Offsets are unsigned: there is no way to name a backward one.
    assert_eq!(
        verify(&[ins(JA, u32::MAX), ret(ALLOW)]),
        Err(Invalid::Jump(0))
    );
}

#[test]
fn rule_6_the_last_instruction_is_a_return() {
    assert_eq!(verify(&[ins(LD_IMM, 1)]), Err(Invalid::NoReturn));
    assert_eq!(
        verify(&[ret(ALLOW), ins(LD_IMM, 1)]),
        Err(Invalid::NoReturn)
    );
}

#[test]
fn rule_7_no_scratch_word_is_read_before_a_store_on_every_path() {
    // Read at once.
    assert_eq!(
        verify(&[ins(LD_MEM, 3), ret(ALLOW)]),
        Err(Invalid::Uninitialised(0))
    );
    assert_eq!(
        verify(&[ins(LDX_MEM, 3), ret(ALLOW)]),
        Err(Invalid::Uninitialised(0))
    );
    // Stored, then read; another word is not the stored one.
    assert!(verify(&[ins(ST, 3), ins(LD_MEM, 3), ret(ALLOW)]).is_ok());
    assert_eq!(
        verify(&[ins(ST, 3), ins(LD_MEM, 4), ret(ALLOW)]),
        Err(Invalid::Uninitialised(1))
    );
    // One path stores and the other does not: the join is the intersection.
    let one_path = [
        Insn::new(JEQ_K, 0, 1, 0), // 0: true -> 1, false -> 2
        ins(ST, 5),                // 1: store, then falls to 2
        ins(LD_MEM, 5),            // 2: reached without a store by the false path
        ret(ALLOW),
    ];
    assert_eq!(verify(&one_path), Err(Invalid::Uninitialised(2)));
    let both_paths = [
        Insn::new(JEQ_K, 0, 2, 0), // 0: true -> 1, false -> 3
        ins(ST, 5),                // 1
        ins(JA, 1),                // 2: skip the other store
        ins(ST, 5),                // 3
        ins(LD_MEM, 5),            // 4
        ret(ALLOW),
    ];
    assert!(verify(&both_paths).is_ok());
}

#[test]
fn rule_7_a_scratch_read_after_a_return_is_judged_by_what_ran_into_it() {
    // Linux's `check_load_and_stores` keeps one running set and never resets
    // it at a `RET`: the instruction after a return takes what ran into the
    // return, narrowed by the jumps that reach it. The first program is the
    // certification consultant's; Linux's own answers are in
    // `tests/data/scratch-<n>.answer`, from `oracle.c --accept`, and
    // `tests/scratch.rs` holds `verify` to them.
    let jeq = |jt, jf| Insn::new(JEQ_K, jt, jf, 0);
    // The jump to the read stored, the fall through the `RET` did not.
    let through_a_return = [
        jeq(2, 0),
        ins(ST, 0),
        ins(JA, 1),
        ret(ALLOW),
        ins(LD_MEM, 0),
        ins(RET_A, 0),
    ];
    assert_eq!(verify(&through_a_return), Err(Invalid::Uninitialised(4)));
    // Code only a `RET` leads to, with nothing stored before it.
    assert_eq!(
        verify(&[ret(ALLOW), ins(LD_MEM, 0), ins(RET_A, 0)]),
        Err(Invalid::Uninitialised(1))
    );
    // A store before the `RET` is still there after it.
    assert!(verify(&[ins(ST, 0), ret(ALLOW), ins(LD_MEM, 0), ins(RET_A, 0)]).is_ok());
}

#[test]
fn a_chain_is_bounded_as_linux_bounds_it() {
    // One filter of 4096 counts 4100; seven of them 28700, and an eighth
    // would total 28700 + 4096 = 32796, over 32768.
    let mut cost = 0;
    let mut chain = 0;
    while fits_path(cost, MAX_INSNS) {
        cost = path_cost(cost, MAX_INSNS);
        chain += 1;
    }
    assert_eq!((chain, cost), (7, 7 * 4100));
    // One-instruction filters count 5 each: the test is the new filter's one
    // plus 5 for each before it, so 6554 are allowed and the 6555th is not.
    let (mut cost, mut chain) = (0, 0);
    while fits_path(cost, 1) {
        cost = path_cost(cost, 1);
        chain += 1;
    }
    assert_eq!(chain, 6554);
    // Exactly at the limit is allowed, one over is not.
    assert!(fits_path(MAX_INSNS_PER_PATH - 10, 10));
    assert!(!fits_path(MAX_INSNS_PER_PATH - 10, 11));
    assert_eq!(MAX_INSNS_PER_PATH, 32768);
}

#[test]
fn a_verified_program_is_kept_whole() {
    let program = [ins(LD_IMM, 5), ins(RET_A, 0)];
    let kept = verify(&program).unwrap();
    assert_eq!(kept.insns(), &program);
    assert_eq!(kept.len(), 2);
    assert!(!kept.is_empty());
}

// ---- run: the machine -----------------------------------------------------

#[test]
fn a_load_reads_the_field_at_its_offset() {
    let data = call(39);
    let word = |at| {
        let program = verified(&[ins(LD_W_ABS, at), ins(RET_A, 0)]);
        run(&program, &data)
    };
    assert_eq!(word(0), 39);
    assert_eq!(word(4), 0xc000_003e);
    assert_eq!(word(8), 0x5566_7788);
    assert_eq!(word(12), 0x1122_3344);
    assert_eq!(word(16), 1);
    assert_eq!(word(20), 0);
    assert_eq!(word(56), u32::MAX);
    assert_eq!(word(60), u32::MAX);
    assert_eq!(data.word(64), None);
    assert_eq!(data.word(2), None);
}

#[test]
fn a_negative_number_is_its_bits() {
    assert_eq!(call(-1).word(0), Some(u32::MAX));
}

#[test]
fn len_loads_are_the_size_of_the_data() {
    assert_eq!(answer(&[ins(LD_W_LEN, 0)]), 64);
    assert_eq!(answer(&[ins(LDX_W_LEN, 0), ins(TXA, 0)]), 64);
}

#[test]
fn arithmetic_is_32_bit_and_wraps() {
    let with = |op: u16, a: u32, k: u32| answer(&[ins(LD_IMM, a), ins(ALU_K | op, k)]);
    assert_eq!(with(ADD, u32::MAX, 2), 1);
    assert_eq!(with(SUB, 1, 2), u32::MAX);
    assert_eq!(with(MUL, 0x1_0001, 0x1_0001), 0x2_0001);
    assert_eq!(with(DIV, 100, 7), 14);
    assert_eq!(with(OR, 0b1100, 0b0110), 0b1110);
    assert_eq!(with(AND, 0b1100, 0b0110), 0b0100);
    assert_eq!(with(XOR, 0b1100, 0b0110), 0b1010);
    assert_eq!(with(LSH, 1, 31), 0x8000_0000);
    assert_eq!(with(RSH, 0x8000_0000, 31), 1);
    assert_eq!(
        answer(&[ins(LD_IMM, 5), ins(ALU_K | NEG, 0)]),
        5_u32.wrapping_neg()
    );
}

#[test]
fn a_shift_by_x_is_masked_to_five_bits() {
    // Linux's interpreter (`kernel/bpf/core.c`, `SHT`) and the hardware agree.
    let shifted = |op: u16, x: u32| answer(&[ins(LDX_IMM, x), ins(LD_IMM, 1), ins(ALU_X | op, 0)]);
    assert_eq!(shifted(LSH, 33), 2);
    assert_eq!(shifted(LSH, 32), 1);
    assert_eq!(shifted(RSH, 64), 1);
}

#[test]
fn a_division_by_an_x_of_zero_ends_the_program_killing() {
    let program = verified(&[
        ins(LDX_IMM, 0),
        ins(LD_IMM, 5),
        ins(ALU_X | DIV, 0),
        ret(ALLOW),
    ]);
    assert_eq!(run(&program, &call(1)), KILL_THREAD);
}

#[test]
fn scratch_words_hold_what_was_stored_and_x_and_a_swap() {
    assert_eq!(
        answer(&[ins(LD_IMM, 7), ins(ST, 3), ins(LD_IMM, 0), ins(LD_MEM, 3)]),
        7
    );
    assert_eq!(
        answer(&[
            ins(LDX_IMM, 9),
            ins(STX, 15),
            ins(LDX_IMM, 0),
            ins(LDX_MEM, 15),
            ins(TXA, 0)
        ]),
        9
    );
    assert_eq!(
        answer(&[ins(LD_IMM, 4), ins(TAX, 0), ins(LD_IMM, 0), ins(TXA, 0)]),
        4
    );
}

#[test]
fn the_tests_take_the_branch_they_name() {
    let branch = |code: u16, a: u32, against: u32| {
        let program = verified(&[
            ins(LD_IMM, a),
            Insn::new(code, 0, 1, against),
            ret(1),
            ret(0),
        ]);
        run(&program, &call(1))
    };
    // true falls to the first return (1), false skips to the second (0).
    assert_eq!(branch(JEQ_K, 5, 5), 1);
    assert_eq!(branch(JEQ_K, 5, 6), 0);
    assert_eq!(branch(JGT_K, 6, 5), 1);
    assert_eq!(branch(JGT_K, 5, 5), 0);
    assert_eq!(branch(JGE_K, 5, 5), 1);
    assert_eq!(branch(JGE_K, 4, 5), 0);
    assert_eq!(branch(JSET_K, 0b101, 0b100), 1);
    assert_eq!(branch(JSET_K, 0b101, 0b010), 0);
    // Unsigned: a high bit is big.
    assert_eq!(branch(JGT_K, 0x8000_0000, 1), 1);
    // Against X.
    let against_x = verified(&[
        ins(LDX_IMM, 7),
        ins(LD_IMM, 7),
        Insn::new(JEQ_X, 0, 1, 0),
        ret(1),
        ret(0),
    ]);
    assert_eq!(run(&against_x, &call(1)), 1);
}

#[test]
fn an_unconditional_jump_skips() {
    assert_eq!(answer(&[ins(LD_IMM, 1), ins(JA, 1), ins(LD_IMM, 2)]), 1);
    assert_eq!(answer(&[ins(LD_IMM, 1), ins(JA, 0), ins(LD_IMM, 2)]), 2);
}

#[test]
fn a_constant_return_ignores_the_accumulator() {
    let program = verified(&[ins(LD_IMM, 9), ret(ERRNO | 1)]);
    assert_eq!(run(&program, &call(1)), ERRNO | 1);
}

#[test]
fn a_filter_answers_by_the_call_number() {
    // if (nr == 39) return ALLOW; return ERRNO | 1;
    let program = verified(&[
        ins(LD_W_ABS, 0),
        Insn::new(JEQ_K, 0, 1, 39),
        ret(ALLOW),
        ret(ERRNO | 1),
    ]);
    assert_eq!(run(&program, &call(39)), ALLOW);
    assert_eq!(run(&program, &call(1)), ERRNO | 1);
}

// ---- the actions ----------------------------------------------------------

#[test]
fn the_most_restrictive_action_wins_and_a_tie_keeps_the_newest() {
    let order = [
        KILL_PROCESS,
        KILL_THREAD,
        TRAP,
        ERRNO,
        USER_NOTIF,
        TRACE,
        LOG,
        ALLOW,
    ];
    for (index, stricter) in order.iter().enumerate() {
        for looser in order.iter().skip(index + 1) {
            assert!(
                more_restrictive(*stricter, *looser),
                "{stricter:#x} {looser:#x}"
            );
            assert!(!more_restrictive(*looser, *stricter));
        }
        assert!(!more_restrictive(*stricter, *stricter));
    }
    // Data does not order actions.
    assert!(!more_restrictive(ERRNO | 2, ERRNO | 1));

    let allow = verified(&[ret(ALLOW)]);
    let eperm = verified(&[ret(ERRNO | 1)]);
    let eacces = verified(&[ret(ERRNO | 13)]);
    let data = call(0);
    let filters = [&allow, &eperm, &eacces];
    assert_eq!(run_all(filters, &data), ERRNO | 1);
    assert_eq!(run_all(filters.into_iter().rev(), &data), ERRNO | 13);
    // No filter at all is never a license: Linux kills the process.
    assert_eq!(run_all([], &data), KILL_PROCESS);
    // An action nobody defined is still ordered: among its neighbours.
    let odd = verified(&[ret(0x0001_0000)]);
    assert_eq!(run_all([&allow, &odd], &data), 0x0001_0000);
}

#[test]
fn an_instruction_is_eight_little_endian_bytes() {
    let bytes = [0x20, 0x00, 0x01, 0x02, 0x04, 0x03, 0x02, 0x01];
    assert_eq!(
        Insn::from_bytes(&bytes),
        Some(Insn::new(0x0020, 1, 2, 0x0102_0304))
    );
    assert_eq!(Insn::from_bytes(&bytes[..7]), None);
    assert_eq!(Insn::from_bytes(&[]), None);
}
