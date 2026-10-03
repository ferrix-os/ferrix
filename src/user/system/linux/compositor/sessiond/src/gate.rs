//! The compositor into its scope before it execs (`docs/AUTH.md` §6.4, the
//! certification consultant's E1): two pipes between the forked child and a
//! thread of `sessiond`'s.
//!
//! The child, between fork and exec, may make only async-signal-safe calls,
//! so it does no more than write its pid and read one byte. The thread asks
//! the init, which may take a while, and answers `y` for a scope made and
//! `n` for one refused.

use std::io::{Read as _, Write as _};
use std::os::fd::{FromRawFd as _, OwnedFd};

/// A pipe, both ends close-on-exec: (read end, write end).
pub(crate) fn pipe() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0; 2];
    #[expect(
        unsafe_code,
        reason = "AUDIT: pipe2 writes two descriptors into an array that lives for the call"
    )]
    // SAFETY: as the reason says.
    let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if made != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let [read, write] = fds;
    Ok((owned(read).into(), owned(write).into()))
}

/// A descriptor `pipe2` just made, owned.
fn owned(fd: i32) -> OwnedFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: pipe2 just made the descriptor, and nothing else owns it"
    )]
    // SAFETY: as the reason says.
    unsafe {
        OwnedFd::from_raw_fd(fd)
    }
}

/// The thread's half: the child's pid from `told`, the scope asked for,
/// the answer down `hear`. The scope's unit, or why there is none.
pub(crate) fn ask_for_scope(
    mut told: &std::fs::File,
    mut hear: &std::fs::File,
    uid: u32,
) -> Result<String, String> {
    let mut pid = [0_u8; 4];
    told.read_exact(&mut pid)
        .map_err(|error| format!("the compositor never said its pid: {error}"))?;
    let scope = crate::scope::join(uid, u32::from_ne_bytes(pid));
    let answer: &[u8] = if scope.is_ok() { b"y" } else { b"n" };
    let _ = hear.write_all(answer);
    scope
}

/// The child's half, between fork and exec: its pid down the first pipe,
/// then the answer. Anything but `y` stops the exec.
pub(crate) fn wait_for_scope((tell, heard): (i32, i32)) -> std::io::Result<()> {
    #[expect(unsafe_code, reason = "AUDIT: getpid takes nothing and cannot fail")]
    // SAFETY: as the reason says.
    let pid = unsafe { libc::getpid() };
    let bytes = u32::try_from(pid).unwrap_or(0).to_ne_bytes();
    #[expect(
        unsafe_code,
        reason = "AUDIT: write of four bytes from an array alive for the call, to a descriptor the child holds"
    )]
    // SAFETY: as the reason says.
    let wrote = unsafe { libc::write(tell, bytes.as_ptr().cast(), bytes.len()) };
    if wrote != 4 {
        return Err(std::io::Error::from_raw_os_error(libc::EPIPE));
    }
    let mut answer = [0_u8; 1];
    #[expect(
        unsafe_code,
        reason = "AUDIT: read of one byte into an array alive for the call, from a descriptor the child holds"
    )]
    // SAFETY: as the reason says.
    let read = unsafe { libc::read(heard, answer.as_mut_ptr().cast(), 1) };
    if read == 1 && answer == *b"y" {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(libc::EPERM))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child's half waits for the answer and goes on only on `y`: run in
    /// this process, with the thread's half played by hand.
    #[test]
    fn the_exec_waits_for_the_scope_and_stops_without_it() {
        for (answer, goes) in [(b"y", true), (b"n", false)] {
            let (told, tell) = pipe().unwrap();
            let (heard, mut hear) = pipe().unwrap();
            hear.write_all(answer).unwrap();
            let gates = (
                std::os::fd::AsRawFd::as_raw_fd(&tell),
                std::os::fd::AsRawFd::as_raw_fd(&heard),
            );
            assert_eq!(wait_for_scope(gates).is_ok(), goes);
            let mut pid = [0_u8; 4];
            (&told).read_exact(&mut pid).unwrap();
            assert_eq!(u32::from_ne_bytes(pid), std::process::id());
        }
        // No answer at all (the thread gone): the exec stops.
        let (_told, tell) = pipe().unwrap();
        let (heard, hear) = pipe().unwrap();
        drop(hear);
        let gates = (
            std::os::fd::AsRawFd::as_raw_fd(&tell),
            std::os::fd::AsRawFd::as_raw_fd(&heard),
        );
        assert!(wait_for_scope(gates).is_err());
    }
}
