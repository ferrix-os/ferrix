//! The waits on a user address (K5, `docs/microkernel/K5.md`): sleep while a
//! 32-bit word holds a value, and wake the waits on a word.
//!
//! A word is named by an address in the caller's memory, or by a VMO handle
//! and a byte offset ([`Word::InVmo`]), which needs no mapping. A wait or wake
//! that asks for a shared word finds every process's waits on the same byte of
//! the same VMO, through whatever mapping or handle each names it; a private
//! one only the caller's own. A woken wait reads its word again: a wake can
//! come from any process that can read the word.

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::types::{
    ADDRESS_BY_VMO, ADDRESS_COMPARE, ADDRESS_SHARED, ADDRESS_WAIT_ANY_MAX, AddressWord,
};

use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_unit};
use crate::handle::{Deadline, register};

/// A bitset that matches every wait.
pub const MATCH_ANY: u32 = u32::MAX;

/// Which word a call names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    /// At this address in the caller's memory, 4-aligned; `shared` keys it by
    /// the VMO a shared mapping of it maps.
    At {
        /// The address.
        address: usize,
        /// Whether to key it by its mapping's VMO.
        shared: bool,
    },
    /// This many bytes into the VMO the handle names, which needs `READ`.
    InVmo {
        /// The VMO.
        vmo: Handle,
        /// The word's byte offset, 4-aligned.
        offset: usize,
    },
}

impl Word {
    /// The address or offset, the flags and the VMO register.
    const fn registers(self) -> (usize, u64, usize) {
        match self {
            Word::At { address, shared } => (address, if shared { ADDRESS_SHARED } else { 0 }, 0),
            Word::InVmo { vmo, offset } => (offset, ADDRESS_BY_VMO, register(vmo)),
        }
    }
}

/// `address_wait`: sleep while `word` holds `expected`, until a wake of it
/// whose bitset shares a bit with `bitset`, or `deadline`.
///
/// # Errors
///
/// [`Error::ShouldWait`] if the word does not hold `expected`;
/// [`Error::TimedOut`]; [`Error::Interrupted`]; [`Error::NoMemory`];
/// [`Error::Fault`]; [`Error::InvalidArgs`]; by handle,
/// [`Error::AccessDenied`] without `READ`.
pub fn wait<S: Syscall>(
    sys: S,
    word: Word,
    expected: u32,
    bitset: u32,
    deadline: Deadline,
) -> Result<(), Error> {
    let (at, flags, vmo) = word.registers();
    let deadline = deadline.bytes();
    decode_unit(
        Call::new(nr::ADDRESS_WAIT)
            .value(at)
            .value(expected as usize)
            .value(bitset as usize)
            .value(flags as usize)
            .optional_input(deadline.as_ref().map(<[u8; 8]>::as_slice))
            .value(vmo)
            .make(sys),
    )
}

/// `address_wake`: wake up to `count` waits on `word`, oldest first, whose
/// bitset shares a bit with `bitset`, and answer how many.
///
/// # Errors
///
/// As [`wait`]'s refusals.
pub fn wake<S: Syscall>(sys: S, word: Word, count: u32, bitset: u32) -> Result<usize, Error> {
    let (at, flags, vmo) = word.registers();
    decode(
        Call::new(nr::ADDRESS_WAKE)
            .value(at)
            .value(count as usize)
            .value(bitset as usize)
            .value(flags as usize)
            .value(vmo)
            .make(sys),
    )
}

/// `address_requeue`: wake up to `wake` waits on the word at `from` and move
/// up to `moved` more onto the word at `to`; with `expected`, only if `from`
/// still holds it. Answers how many were woken or moved.
///
/// # Errors
///
/// [`Error::ShouldWait`] if `from` does not hold `expected`; as [`wait`]'s.
pub fn requeue<S: Syscall>(
    sys: S,
    (from, to): (usize, usize),
    (wake, moved): (u32, u32),
    shared: bool,
    expected: Option<u32>,
) -> Result<usize, Error> {
    let mut flags = if shared { ADDRESS_SHARED } else { 0 };
    if expected.is_some() {
        flags |= ADDRESS_COMPARE;
    }
    decode(
        Call::new(nr::ADDRESS_REQUEUE)
            .value(from)
            .value(to)
            .value(wake as usize)
            .value(moved as usize)
            .value(flags as usize)
            .value(expected.unwrap_or(0) as usize)
            .make(sys),
    )
}

/// `address_wait_any`: sleep on every one of `words` that holds its expected
/// value until a wake of any of them, and answer its index.
///
/// # Errors
///
/// [`Error::InvalidArgs`] for no words or more than `ADDRESS_WAIT_ANY_MAX`;
/// as [`wait`]'s.
pub fn wait_any<S: Syscall>(
    sys: S,
    words: &[AddressWord],
    deadline: Deadline,
) -> Result<usize, Error> {
    if words.is_empty() || words.len() > ADDRESS_WAIT_ANY_MAX {
        return Err(Error::InvalidArgs);
    }
    let mut vector = [0_u8; ADDRESS_WAIT_ANY_MAX * 16];
    for (word, element) in words.iter().zip(vector.chunks_exact_mut(16)) {
        let (address, rest) = element.split_at_mut(8);
        let (expected, flags) = rest.split_at_mut(4);
        address.copy_from_slice(&word.address.to_ne_bytes());
        expected.copy_from_slice(&word.expected.to_ne_bytes());
        flags.copy_from_slice(&word.flags.to_ne_bytes());
    }
    let used = vector.get(..words.len() * 16).unwrap_or(&[]);
    let deadline = deadline.bytes();
    decode(
        Call::new(nr::ADDRESS_WAIT_ANY)
            .input(used)
            .value(words.len())
            .value(0)
            .optional_input(deadline.as_ref().map(<[u8; 8]>::as_slice))
            .make(sys),
    )
}
