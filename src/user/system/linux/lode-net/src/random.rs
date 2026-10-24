//! Random numbers: the kernel's, through `getrandom(2)`, and nothing else.
//!
//! On Ferrix that is the kernel's ChaCha20 generator, seeded at boot from
//! firmware's and the processor's random bytes (`src/kernel/src/random.rs`);
//! on Linux, the kernel's pool. There is no seeding in user space and no
//! fallback to `/dev/urandom` or anything else: a read that fails is an
//! error the caller turns into a failed handshake or a refused key.

/// Fill `bytes` from the kernel, all of them.
///
/// # Errors
///
/// The `errno` of a `getrandom` that failed (other than `EINTR`, which is
/// tried again).
pub fn fill(bytes: &mut [u8]) -> std::io::Result<()> {
    let mut done = 0;
    while let Some(rest) = bytes.get_mut(done..) {
        if rest.is_empty() {
            break;
        }
        // SAFETY: `rest` is writable for its length for the call; no flag
        // asks for anything but the blocking, fully seeded pool.
        let got = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if got < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if got == 0 {
            return Err(std::io::Error::other("getrandom gave nothing"));
        }
        done += got.unsigned_abs();
    }
    Ok(())
}
