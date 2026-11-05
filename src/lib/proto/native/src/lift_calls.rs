//! The calls M5's shim makes that nobody else has (`docs/microkernel/M5-LIFT.md` §2).

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;

use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_unit};
use crate::handle::register;

/// `DEBUG_WRITE`: `bytes` to the kernel's write-only debug log, through its handle.
pub fn debug_write<S: Syscall>(sys: S, log: Handle, bytes: &[u8]) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::DEBUG_WRITE)
            .value(register(log))
            .input(bytes)
            .value(bytes.len())
            .make(sys),
    )
}
/// `THREAD_YIELD`.
pub fn thread_yield<S: Syscall>(sys: S) -> Result<(), Error> {
    decode_unit(Call::new(nr::THREAD_YIELD).make(sys))
}
/// `CLOCK_READ`.
pub fn clock_read<S: Syscall>(sys: S, clock: usize) -> Result<u64, Error> {
    decode(Call::new(nr::CLOCK_READ).value(clock).make(sys)).map(|value| value as u64)
}
/// `THREAD_AFFINITY`: thread 0 is the caller.
pub fn thread_affinity<S: Syscall>(sys: S, thread: u32, mask: &[u8]) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::THREAD_AFFINITY)
            .value(thread as usize)
            .input(mask)
            .value(mask.len() / 8)
            .make(sys),
    )
}
/// `BOOTSTRAP_MOVE`: this process's private `handle` to `process`'s bootstrap slot, or
/// with `shared` one of the region's own, a channel linuxd itself made for its pid 1.
pub fn bootstrap_move<S: Syscall>(
    sys: S,
    process: Handle,
    handle: Handle,
    shared: bool,
) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::BOOTSTRAP_MOVE)
            .value(register(process))
            .value(register(handle))
            .value(usize::from(shared))
            .make(sys),
    )
}
/// `SPACE_WINDOW`: `space`'s pages are placed from here on.
pub fn space_window<S: Syscall>(sys: S, space: Handle) -> Result<(), Error> {
    decode_unit(Call::new(nr::SPACE_WINDOW).value(register(space)).make(sys))
}
/// `SPACE_PLACE`: `entries`, 16 bytes each (`ferrix_native_abi::lift`): how many went in.
pub fn space_place<S: Syscall>(
    sys: S,
    space: Handle,
    vmo: Handle,
    entries: &[u8],
) -> Result<usize, Error> {
    decode(
        Call::new(nr::SPACE_PLACE)
            .value(register(space))
            .value(register(vmo))
            .input(entries)
            .value(entries.len() / 16)
            .make(sys),
    )
}
/// `THREAD_RESERVE`.
pub fn thread_reserve<S: Syscall>(
    sys: S,
    job: Handle,
    runtime: u64,
    deadline: u64,
    period: u64,
) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::THREAD_RESERVE)
            .value(register(job))
            .value(runtime as usize)
            .value(deadline as usize)
            .value(period as usize)
            .make(sys),
    )
}
/// `RANDOM_FILL`.
pub fn random_fill<S: Syscall>(sys: S, out: &mut [u8]) -> Result<(), Error> {
    let length = out.len();
    decode_unit(
        Call::new(nr::RANDOM_FILL)
            .output(out)
            .value(length)
            .make(sys),
    )
}
/// `CONSOLE_READ`, not waiting.
pub fn console_read<S: Syscall>(sys: S, console: Handle, out: &mut [u8]) -> Result<usize, Error> {
    let length = out.len();
    decode(
        Call::new(nr::CONSOLE_READ)
            .value(register(console))
            .output(out)
            .value(length)
            .make(sys),
    )
}
/// `OBJECT_KIND`: what a handle names, and its rights.
pub fn object_kind<S: Syscall>(sys: S, handle: Handle) -> Result<(u64, u32), Error> {
    let word = decode(Call::new(nr::OBJECT_KIND).value(register(handle)).make(sys))? as u64;
    Ok((word >> 32, word as u32))
}
