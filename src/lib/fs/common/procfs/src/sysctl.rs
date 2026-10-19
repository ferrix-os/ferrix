//! The values under `/proc/sys`.
//!
//! ```text
//! Linux
//! 4194304
//! ```
//!
//! Every file there is one value and a newline, which is what `sysctl`
//! prints after `key = ` and what a shell's `$(cat …)` strips. Strings are
//! `proc_dostring`'s and numbers `proc_dointvec`'s and `proc_doulongvec`'s,
//! from `kernel/sysctl.c`; a file holding a vector of numbers separates them
//! with tabs, and none here does.

use alloc::vec::Vec;

use crate::text::put;

/// Append a string value: its bytes, then a newline.
pub fn string(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(value);
    out.push(b'\n');
}

/// Append a number, in decimal, then a newline.
pub fn number(out: &mut Vec<u8>, value: u64) {
    put(out, format_args!("{value}\n"));
}

/// What a write of `data` at the start of a string value stores.
///
/// `_proc_do_string` copies byte by byte and stops at the first NUL or
/// newline, so `echo name > hostname` stores `name`, and it stops at `max`
/// bytes without saying so: a longer write is cut, not refused. The whole
/// write counts as consumed either way.
#[must_use]
pub fn stored(data: &[u8], max: usize) -> &[u8] {
    let end = data
        .iter()
        .position(|&byte| byte == 0 || byte == b'\n')
        .unwrap_or(data.len())
        .min(max);
    data.get(..end).unwrap_or(data)
}
