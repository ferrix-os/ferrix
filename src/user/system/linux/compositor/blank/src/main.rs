//! Iteration 1 of the compositor: fill the screen with one colour.
//!
//! `docs/DISPLAY.md` puts a blank screen on Ferrix first, so the path from a
//! Linux program through `/dev/dri/card0`, the kernel's display core and the
//! ring-3 virtio-gpu driver to QEMU's window is proven before a compositor
//! draws anything on it. This program is that proof, and it uses nothing a
//! real compositor's DRM backend does not: the legacy mode-setting calls and
//! a dumb buffer.
//!
//! It runs as init in the display test, so it never exits: it prints one
//! line saying what it showed, or where it failed, and waits.

#[cfg(target_os = "linux")]
fn main() {
    use std::io::Write;

    use compositor_drm::{Card, bystander, isolation, master, probe, show};

    let mut out = std::io::stdout();
    // The render node first, and always: the display test watches the boot
    // for the compositor's own prefix and reads back from it, so this line
    // has a prefix of its own and is already printed when that one comes.
    // A card with no GPU behind it says so here rather than failing.
    let _ = writeln!(out, "{}", probe());
    // Before anything is shown, on opens of its own that it closes: whether
    // an open maps its own buffers and nothing else (E-02). Its own prefix
    // too, for the same reason.
    let _ = writeln!(out, "{}", isolation());
    // Then who shows things: two opens, the master's rules, and a screen
    // the test reads while nobody is master (`compositor_drm::master`).
    let _ = writeln!(out, "{}", master());
    let _ = out.flush();
    let line = match Card::open().and_then(|card| show(&card)) {
        Ok(line) => line,
        Err(error) => format!("compositor: failed: {error}"),
    };
    // A program that only reads the card opens and closes it while the
    // screen is up: its close must not take the master's frame off, which
    // the test's screendump after the line below shows.
    bystander();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    // As init, exiting would end the machine; as a program, the scanout lasts
    // only while the card is open.
    loop {
        // SAFETY: pause has no preconditions.
        let _ = unsafe { libc::pause() };
    }
}

/// Only Linux, and Ferrix through its Linux ABI, have `/dev/dri`; elsewhere the
/// crate builds so its modeset logic is tested on any host.
#[cfg(not(target_os = "linux"))]
fn main() {}
