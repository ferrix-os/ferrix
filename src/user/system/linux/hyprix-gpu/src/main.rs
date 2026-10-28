//! hyprix-gpu's start: the socket hyprix handed it, the device, and the
//! conversation until hyprix closes its end (`docs/GPU-COMPOSITOR.md` §4,
//! §9).
//!
//! It takes no arguments. `HYPRIX_GPU_FD` names its end of the socket
//! (3 by default), `HYPRIX_GPU_NODE` the render node whose GPU it should
//! draw on, and `HYPRIX_GPU_DEVICE`, when set, a part of the name of the
//! Vulkan device to take instead (a test's lavapipe).

use std::os::fd::{FromRawFd as _, OwnedFd};
use std::process::ExitCode;

use hyprix_gpu::server::{End, Server, warn};
use hyprix_gpu::vulkan::Vulkan;
use hyprix_gpu::wire::Wire;

/// The render node's device number, major and minor.
fn node(path: &str) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    Some((libc::major(rdev), libc::minor(rdev)))
}

fn main() -> ExitCode {
    let fd = std::env::var("HYPRIX_GPU_FD")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(3);
    // SAFETY: hyprix hands the helper this descriptor, open, for it alone.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    let near = std::env::var("HYPRIX_GPU_NODE")
        .ok()
        .and_then(|path| node(&path));
    let wanted = std::env::var("HYPRIX_GPU_DEVICE").ok();
    let core = match tole::gpu::Core::new(false, wanted, near) {
        Ok(core) => core,
        Err(why) => {
            warn(&format!("no device: {why}"));
            return ExitCode::FAILURE;
        }
    };
    let driver = if core.is_software() {
        "lavapipe"
    } else {
        "vulkan"
    }
    .to_owned();
    let name = core.name().to_owned();
    let vulkan = match Vulkan::new(core) {
        Ok(vulkan) => vulkan,
        Err(why) => {
            warn(&format!("{name}: {why}"));
            return ExitCode::FAILURE;
        }
    };
    warn(&format!("drawing on {name}"));
    let mut server = Server::new(vulkan, Wire::new(socket), driver);
    match server.run() {
        End::Closed => ExitCode::SUCCESS,
        End::Lost(why) => {
            warn(&format!("the device was lost: {why}"));
            ExitCode::FAILURE
        }
        End::Socket(error) => {
            warn(&format!("the socket: {error}"));
            ExitCode::FAILURE
        }
    }
}
