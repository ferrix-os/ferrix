//! The runtime a Ferrix native program links.
//!
//! Everything a native program needs that cannot be a pure function, and so
//! cannot live in `src/lib/`:
//!
//! * `_start`, where the kernel enters the process, with the bootstrap handle
//!   in the first argument register;
//! * [`Kernel`], the [`Syscall`] that traps into the kernel with the
//!   architecture's instruction;
//! * [`exit`], and a panic handler that exits with [`PANIC_STATUS`];
//! * [`entry!`], which names a program's `main`;
//! * `linker/native.ld`, the layout the kernel's ELF loader accepts.
//!
//! The calls themselves — typed, owned handles and decoded errors — are
//! `src/lib/proto/native`, re-exported here as [`native`], so a program depends on this
//! crate alone.
//!
//! # A program
//!
//! ```text
//! #![no_std]
//! #![no_main]
//!
//! use ferrix_rt::{Bootstrap, native::channel};
//!
//! ferrix_rt::entry!(main);
//!
//! fn main(bootstrap: Bootstrap) -> i32 {
//!     match channel::create(ferrix_rt::Kernel) { Ok(_) => 0, Err(_) => 1 }
//! }
//! ```
//!
//! `main`'s return value is the process's exit status. Its build script links
//! it with the runtime's linker script:
//!
//! ```text
//! println!("cargo::rustc-link-arg-bins=-T{}", env!("DEP_FERRIX_RT_LINKER_SCRIPT"));
//! ```
//!
//! # The bootstrap handle
//!
//! `process_start` (0x1031, not yet on main) installs one handle in the new
//! process as its first, `Handle(1)`, and passes its value in the first
//! argument register, which `_start` hands on untouched. Started any other
//! way — by `execve`, which zeroes every register — the register is zero, and
//! `main` is given `None`.

#![no_std]

mod arch;
pub mod linux;

pub use ferrix_native as native;
use ferrix_native::channel::Channel;
use ferrix_native::{Handle, OwnedHandle, Raw, Syscall};

/// The exit status a panic ends the process with: 101, as Rust's `std` uses.
pub const PANIC_STATUS: i32 = 101;

/// What `main` is given: the channel the process was started with, if any.
pub type Bootstrap = Option<Channel<Kernel>>;

/// The kernel, reached by trapping into it.
///
/// Zero-sized, so every handle carries one for nothing. Anyone may make one:
/// the only thing it can do is make a [`Raw`] call, and only `src/lib/proto/native`
/// can build one of those.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kernel;

impl Syscall for Kernel {
    fn call(self, raw: Raw<'_>) -> usize {
        arch::call(&raw)
    }

    fn call_words(self, raw: Raw<'_>) -> Option<(usize, [usize; 3])> {
        Some(arch::trap_words(raw.number(), raw.args()))
    }
}

/// End the process with `status`.
///
/// Linux's `exit_group`, because the native ABI has no exit of its own: a
/// native program is a process like any other, and the kernel's dispatcher
/// ends one the same way whichever ABI it speaks.
pub fn exit(status: i32) -> ! {
    arch::exit(status)
}

/// The processor's free-running counter, when a program may read it: the
/// TSC on x86-64, the virtual counter on AArch64, and `None` on ARMv7-A,
/// where the kernel does not let user mode read it. For a driver timing its
/// own device, as the block driver does for the seam's measurement.
#[must_use]
pub fn counter() -> Option<u64> {
    arch::counter()
}

/// Complete every memory and register access before this before any after
/// it, as a device sees them: what a driver sharing memory with a device
/// that does not snoop -- mapped past the caches by a coherent pin --
/// needs between writing a descriptor and letting the device read it.
pub fn device_barrier() {
    arch::device_barrier();
}

/// Run a program: adopt the bootstrap handle, call `main`, exit with what it
/// returns. [`entry!`] calls this; nothing else should.
pub fn start(bootstrap: usize, main: fn(Bootstrap) -> i32) -> ! {
    let handle = u64::try_from(bootstrap).map_or(Handle::INVALID, Handle::from_register);
    let bootstrap = handle
        .is_valid()
        .then(|| Channel::from_owned(OwnedHandle::from_raw(Kernel, handle)));
    exit(main(bootstrap))
}

/// Name a program's `main`, a `fn(Bootstrap) -> i32`.
///
/// Defines `ferrix_rt_start`, the symbol `_start` calls with the bootstrap
/// register. A program that forgets this fails to link, by name.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        /// The first Rust function the process runs, called by `_start`.
        #[unsafe(no_mangle)]
        extern "C" fn ferrix_rt_start(bootstrap: usize) -> ! {
            $crate::start(bootstrap, $main)
        }
    };
}

/// A panic ends the process with [`PANIC_STATUS`].
///
/// The one panic handler, and the only one there can be: a `no_std` program
/// must define exactly one. It says where it panicked and why on standard
/// error, which a driver has open on the console, as one line -- on a machine
/// whose screen is its only output that line is the whole diagnosis -- and the
/// status is what a parent waiting on the process can see. The workspace
/// lints deny every way of reaching it in production code, so a panic here is
/// a bug the lints could not see, such as an arithmetic overflow.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write as _;
    let mut line = PanicLine {
        bytes: [0; 200],
        len: 0,
    };
    let _ = line.write_str("panic");
    if let Some(location) = info.location() {
        let _ = write!(line, " at {}:{}", location.file(), location.line());
    }
    let _ = write!(line, ": {}", info.message());
    let _ = line.write_str("\n");
    let _ = linux::write(2, line.bytes.get(..line.len).unwrap_or(&[]));
    exit(PANIC_STATUS)
}

/// The panic's line, cut at its buffer's end.
struct PanicLine {
    bytes: [u8; 200],
    len: usize,
}

impl core::fmt::Write for PanicLine {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}
