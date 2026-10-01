//! Classic BPF as `seccomp(2)` runs it: a filter is verified once, when it is
//! installed, and run against a `struct seccomp_data` at every system call,
//! answering the action to take (`docs/SECCOMP.md` §3.1).
//!
//! # The shape
//!
//! * [`verify`] is Linux's `bpf_check_classic` followed by its
//!   `seccomp_check_filter`, rule for rule, and answers a [`Program`] -- the
//!   only way to make one, so a `Program` is a program that passed.
//! * [`run`] is the classic machine over a [`SeccompData`]. It never fails
//!   and never loops: the program counter only moves forward, so a verified
//!   program of `n` instructions stops within `n` steps, and the loop is
//!   bounded by the length as well.
//! * [`run_all`] runs every filter a process holds, newest first, and answers
//!   the most restrictive result ([`more_restrictive`]).
//!
//! No `unsafe`, and nothing in the run path allocates.
//!
//! # Byte order
//!
//! `struct seccomp_data` is host-endian. Every target Ferrix has is
//! little-endian; a filter loads a 32-bit word of the structure, and
//! [`SeccompData::word`] is the word of its fields as little-endian bytes.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod machine;
mod verify;

#[cfg(test)]
mod tests;

pub use machine::{run, run_all, run_counted};
pub use verify::{Invalid, Program, verify};

/// Most instructions in one filter: Linux's `BPF_MAXINSNS`.
pub const MAX_INSNS: usize = 4096;

/// Most instructions across all the filters of one thread, each filter counted
/// with four more: Linux's `MAX_INSNS_PER_PATH`, which is `1 << 18` *bytes* of
/// `struct sock_filter`, 32768 of them. (S1 had it as `1 << 18` instructions,
/// eight times too many; the certification consultant found it.)
pub const MAX_INSNS_PER_PATH: usize = (1 << 18) / INSN_BYTES;

/// What each filter of a chain counts for beyond its own instructions, against
/// [`MAX_INSNS_PER_PATH`]: Linux's `+ 4` in `seccomp_attach_filter`.
pub const FILTER_OVERHEAD: usize = 4;

/// What a chain counts against [`MAX_INSNS_PER_PATH`] once a filter of
/// `new_len` instructions joins one that already counts `earlier`: the new
/// filter's length and its four. Linux refuses the new filter, `ENOMEM`, when
/// its own length plus every earlier filter's length and four is more than the
/// limit ([`fits_path`]).
#[must_use]
pub const fn path_cost(earlier: usize, new_len: usize) -> usize {
    earlier
        .saturating_add(new_len)
        .saturating_add(FILTER_OVERHEAD)
}

/// Whether a filter of `new_len` instructions may join a chain that counts
/// `earlier`: the refusal `total_insns > MAX_INSNS_PER_PATH`, the other way
/// round. The new filter's own four are not part of the test, as in Linux.
#[must_use]
pub const fn fits_path(earlier: usize, new_len: usize) -> bool {
    earlier.saturating_add(new_len) <= MAX_INSNS_PER_PATH
}

/// Words of scratch memory: `BPF_MEMWORDS`.
pub const MEMWORDS: usize = 16;

/// Bytes in `struct seccomp_data`, which `LD|W|LEN` and `LDX|W|LEN` read as.
pub const DATA_BYTES: usize = 64;

/// Bytes in one `struct sock_filter`.
pub const INSN_BYTES: usize = 8;

/// `SECCOMP_RET_KILL_PROCESS`: end the whole process with `SIGSYS`.
pub const KILL_PROCESS: u32 = 0x8000_0000;
/// `SECCOMP_RET_KILL_THREAD` (also `SECCOMP_RET_KILL`): end the thread.
pub const KILL_THREAD: u32 = 0x0000_0000;
/// `SECCOMP_RET_TRAP`: send `SIGSYS`.
pub const TRAP: u32 = 0x0003_0000;
/// `SECCOMP_RET_ERRNO`: fail the call with the data as its errno.
pub const ERRNO: u32 = 0x0005_0000;
/// `SECCOMP_RET_USER_NOTIF`: ask a supervisor, which Ferrix has none of.
pub const USER_NOTIF: u32 = 0x7fc0_0000;
/// `SECCOMP_RET_TRACE`: ask a tracer, which Ferrix has none of.
pub const TRACE: u32 = 0x7ff0_0000;
/// `SECCOMP_RET_LOG`: allow, and log.
pub const LOG: u32 = 0x7ffc_0000;
/// `SECCOMP_RET_ALLOW`.
pub const ALLOW: u32 = 0x7fff_0000;
/// The action half of a result: `SECCOMP_RET_ACTION_FULL`.
pub const ACTION_FULL: u32 = 0xffff_0000;
/// The data half of a result: `SECCOMP_RET_DATA`.
pub const DATA: u32 = 0x0000_ffff;

/// One `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    /// The opcode.
    pub code: u16,
    /// Jump this far on a true test.
    pub jt: u8,
    /// Jump this far on a false one.
    pub jf: u8,
    /// A constant, an offset or a jump distance.
    pub k: u32,
}

impl Insn {
    /// An instruction from its four fields.
    #[must_use]
    pub const fn new(code: u16, jt: u8, jf: u8, k: u32) -> Insn {
        Insn { code, jt, jf, k }
    }

    /// Read one from its eight bytes, as a program's `sock_filter` array
    /// holds it: `code`, `jt`, `jf`, `k`, little-endian.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Insn> {
        let &[c0, c1, jt, jf, k0, k1, k2, k3] = bytes.get(..INSN_BYTES)? else {
            return None;
        };
        Some(Insn {
            code: u16::from_le_bytes([c0, c1]),
            jt,
            jf,
            k: u32::from_le_bytes([k0, k1, k2, k3]),
        })
    }
}

/// `struct seccomp_data`: what a filter judges a call by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeccompData {
    /// The system call's number, as the entry's own register holds it (the
    /// low 32 bits), at offset 0.
    pub nr: i32,
    /// The `AUDIT_ARCH_*` of the entry the call came through, at offset 4.
    pub arch: u32,
    /// The instruction after the call, at offset 8.
    pub instruction_pointer: u64,
    /// The six argument registers, at offsets 16 to 63.
    pub args: [u64; 6],
}

impl SeccompData {
    /// The 32-bit word at the aligned byte offset `at`, or `None` outside the
    /// structure or unaligned: what `LD|W|ABS` loads.
    #[must_use]
    pub fn word(&self, at: u32) -> Option<u32> {
        if !at.is_multiple_of(4) {
            return None;
        }
        let half = |value: u64, high: bool| {
            if high {
                (value >> 32) as u32
            } else {
                value as u32
            }
        };
        match at {
            0 => Some(self.nr as u32),
            4 => Some(self.arch),
            8 | 12 => Some(half(self.instruction_pointer, at == 12)),
            16..=60 => {
                let index = usize::try_from((at - 16) / 8).ok()?;
                self.args.get(index).map(|&arg| half(arg, at % 8 == 4))
            }
            _ => None,
        }
    }
}

/// Whether result `a` is more restrictive than `b`: Linux compares the action
/// halves as signed numbers, so `KILL_PROCESS` (negative) is the most
/// restrictive, then `KILL_THREAD`, `TRAP`, `ERRNO`, `USER_NOTIF`, `TRACE`,
/// `LOG` and `ALLOW`; an action nobody defined sorts among them by its value.
#[must_use]
pub const fn more_restrictive(a: u32, b: u32) -> bool {
    ((a & ACTION_FULL) as i32) < ((b & ACTION_FULL) as i32)
}
