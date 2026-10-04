//! The keymap `wl_keyboard.keymap` hands over, read as text.

use std::os::fd::{AsRawFd as _, OwnedFd};

/// The keymap's text, mapped read-only rather than read.
///
/// A descriptor that arrived over a socket shares its file offset with the
/// compositor's own, so reading it would move the compositor's offset and
/// leave the next client an empty keymap; the protocol says a client maps
/// it, and that is why (the term app found this first).
pub(crate) fn read(fd: &OwnedFd, size: usize) -> Option<String> {
    if size == 0 {
        return None;
    }
    #[expect(
        unsafe_code,
        reason = "AUDIT: mmap is not in std; a read-only private map of a descriptor this client owns, checked against MAP_FAILED"
    )]
    // SAFETY: a null hint lets the kernel choose the address.
    let address = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd.as_raw_fd(),
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return None;
    }
    #[expect(
        unsafe_code,
        reason = "AUDIT: the mapping is `size` bytes by construction and read as bytes, which any pattern is"
    )]
    // SAFETY: `size` bytes were just mapped at `address`.
    let mapped = unsafe { core::slice::from_raw_parts(address.cast::<u8>(), size) };
    // The length counts the terminating NUL.
    let text = mapped
        .get(..size.saturating_sub(1))
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .map(str::to_owned);
    #[expect(
        unsafe_code,
        reason = "AUDIT: unmapping exactly the mapping made above"
    )]
    // SAFETY: the address and length are the ones just mapped.
    let _ = unsafe { libc::munmap(address, size) };
    text
}
