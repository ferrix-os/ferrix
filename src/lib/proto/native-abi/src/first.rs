//! What the kernel writes on a first process's bootstrap channel
//! (`docs/MICROKERNEL.md` §5, `docs/microkernel/BOOT-AUTHORITY.md` §3): the
//! partition manager's. `devmgr` keeps DEVICES, and `linuxd` its boot page.
//!
//! One message, sixteen bytes and its handles: the magic, the version, how many
//! images, how many devices, and then, in this order, the job, the debug log, the
//! description, each partition's image and each partition's devices, the last
//! two in the description's order.

/// The message's first word.
pub const FIRST_MAGIC: u32 = u32::from_le_bytes(*b"FXBA");
/// The one version.
pub const FIRST_VERSION: u32 = 1;
/// Bytes of the message.
pub const FIRST_BYTES: usize = 16;
/// The handles before the images: the job, the debug log, the description.
pub const FIRST_FIXED_HANDLES: usize = 3;
/// READY's first word: the manager to the kernel, once every partition it is to
/// start has started. Started, not healthy.
pub const READY_MAGIC: u32 = u32::from_le_bytes(*b"FXRY");

/// The message: how many images and devices follow the fixed handles.
#[must_use]
pub fn encode(images: u32, devices: u32) -> [u8; FIRST_BYTES] {
    let mut out = [0; FIRST_BYTES];
    for (slot, word) in out
        .chunks_exact_mut(4)
        .zip([FIRST_MAGIC, FIRST_VERSION, images, devices])
    {
        slot.copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// How many images and devices `bytes` says; `None` for any other bytes.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() != FIRST_BYTES {
        return None;
    }
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|four| <[u8; 4]>::try_from(four).ok())
            .map(u32::from_le_bytes)
    };
    (word(0)? == FIRST_MAGIC && word(4)? == FIRST_VERSION).then_some(())?;
    Some((word(8)?, word(12)?))
}
