//! What the kernel says to pid 1 before pid 1 runs (`docs/INIT.md` §6, K2).
//!
//! The kernel starts init with a bootstrap channel, as it starts `devmgr`,
//! and writes one message on its own end first. Init takes its end with
//! `process_bootstrap` ([`crate::nr::PROCESS_BOOTSTRAP`]) and reads the
//! message with `channel_read`. The kernel keeps its end open for as long as
//! the program runs, so later versions can carry what version 1 does not:
//! a power handle, and in a microkernel the device root and physical memory.
//!
//! # The header, which never changes
//!
//! Eight bytes: the magic `FXIN`, then the version as a little-endian `u32`.
//! Little-endian on every target rather than native, so the bytes are the
//! same on all three and a reader needs no word of the kernel's to parse
//! them. What follows the header, and which handles the message carries, is
//! the version's to say.
//!
//! # Version 1
//!
//! The header alone: [`INIT_HELLO_BYTES`] bytes and no handles. A reader
//! that knows version 1 accepts any version from 1 up and reads the header
//! only, since a later version adds after it and never changes it.
//!
//! # After the hello: devmgr's starter, and where `/` is
//!
//! Under `ferrix.devmgr=init` (`docs/INIT.md` §7.3, L12) two more messages
//! follow on the same channel, each eight bytes in the header's shape, a
//! magic and a little-endian `u32`:
//!
//! * [`DEVMGR_STARTER_MAGIC`], written straight after the hello, carries one
//!   handle: the one-shot capability `devmgr_start` takes
//!   ([`crate::nr::NativeCall::DevmgrStart`]), with `MANAGE` and nothing
//!   else, so it can never leave pid 1's table. Its `u32` is 1.
//! * [`ROOT_MAGIC`], written once the kernel has decided where `/` is, when
//!   the root disk's driver has published it or it is known there is none.
//!   Its `u32` is [`ROOT_SWITCHED`] when `/` is now the root volume and pid 1
//!   has been moved onto it, and [`ROOT_IN_MEMORY`] when `/` stays the
//!   tmpfs. No handles.
//!
//! A boot without the option writes neither.
//!
//! # The audit record's reader
//!
//! [`AUDIT_MAGIC`], written after the hello on every boot, carries one
//! handle: the audit record's (`docs/certification/AUDIT.md` §4), with
//! `READ` and nothing else, so it can never leave pid 1's table. Its `u32`
//! is 1. Only the first program is given it.
//!
//! # The processor service token
//!
//! [`CPU_SERVICE_MAGIC`], written after the audit record's on every boot,
//! carries one handle: the processor service token
//! (`docs/CORE-QUARANTINE.md` §4), with `MANAGE`, `DUPLICATE` and
//! `TRANSFER`, so pid 1 keeps it and gives lode a copy with `MANAGE` and
//! `TRANSFER`, again whenever lode restarts. Its `u32` is 1. Only the first
//! program is given it.

/// The first four bytes of the message.
pub const INIT_HELLO_MAGIC: [u8; 4] = *b"FXIN";

/// The version this crate describes.
pub const INIT_HELLO_VERSION: u32 = 1;

/// The header's length, which is the whole of a version 1 message.
pub const INIT_HELLO_BYTES: usize = 8;

/// How many handles a version 1 message carries.
pub const INIT_HELLO_HANDLES: usize = 0;

/// The version 1 message, as the kernel writes it.
#[must_use]
pub const fn init_hello() -> [u8; INIT_HELLO_BYTES] {
    let [m0, m1, m2, m3] = INIT_HELLO_MAGIC;
    let [v0, v1, v2, v3] = INIT_HELLO_VERSION.to_le_bytes();
    [m0, m1, m2, m3, v0, v1, v2, v3]
}

/// The version a message read on pid 1's bootstrap channel says it is, or
/// `None` for one that is not the kernel's: shorter than the header, or
/// without the magic, or version zero.
#[must_use]
pub fn init_hello_version(message: &[u8]) -> Option<u32> {
    let (magic, rest) = message.split_first_chunk::<4>()?;
    let (version, _) = rest.split_first_chunk::<4>()?;
    let version = u32::from_le_bytes(*version);
    (*magic == INIT_HELLO_MAGIC && version != 0).then_some(version)
}

/// The magic of the message carrying `devmgr`'s starter.
pub const DEVMGR_STARTER_MAGIC: [u8; 4] = *b"FXDS";

/// The magic of the message carrying the audit record's handle.
pub const AUDIT_MAGIC: [u8; 4] = *b"FXAU";

/// The magic of the message carrying the processor service token.
pub const CPU_SERVICE_MAGIC: [u8; 4] = *b"FXCS";

/// The magic of the message saying where `/` is.
pub const ROOT_MAGIC: [u8; 4] = *b"FXRT";

/// [`ROOT_MAGIC`]'s value when `/` stays the tmpfs.
pub const ROOT_IN_MEMORY: u32 = 0;

/// [`ROOT_MAGIC`]'s value when `/` is the root volume, and pid 1 is on it.
pub const ROOT_SWITCHED: u32 = 1;

/// One of the messages after the hello, as the kernel writes it: `magic`
/// and `value`.
#[must_use]
pub const fn after_hello(magic: [u8; 4], value: u32) -> [u8; INIT_HELLO_BYTES] {
    let [m0, m1, m2, m3] = magic;
    let [v0, v1, v2, v3] = value.to_le_bytes();
    [m0, m1, m2, m3, v0, v1, v2, v3]
}

/// What a message after the hello says: its magic and its value, or `None`
/// for one that is not eight bytes.
#[must_use]
pub fn read_after_hello(message: &[u8]) -> Option<([u8; 4], u32)> {
    let bytes: [u8; INIT_HELLO_BYTES] = message.try_into().ok()?;
    let (magic, value) = bytes.split_first_chunk::<4>()?;
    let value: [u8; 4] = value.try_into().ok()?;
    Some((*magic, u32::from_le_bytes(value)))
}
