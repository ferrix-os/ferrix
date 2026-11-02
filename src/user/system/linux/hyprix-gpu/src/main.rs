//! hyprix-gpu's start: the socket hyprix handed it, the device, and the
//! conversation until hyprix closes its end (`docs/GPU-COMPOSITOR.md` §4,
//! §9).
//!
//! It takes no arguments. `HYPRIX_GPU_FD` names its end of the socket
//! (3 by default), `HYPRIX_GPU_NODE` the render node whose GPU it should
//! draw on, and `HYPRIX_GPU_DEVICE`, when set, a part of the name of the
//! Vulkan device to take instead (a test's lavapipe). In an image that
//! carries it (`/usr/lib/hyprix-gpu`), the loader is pointed at that
//! prefix's manifests alone, unless `VK_DRIVER_FILES` or
//! `VK_ICD_FILENAMES` says otherwise.

use std::os::fd::{FromRawFd as _, OwnedFd};
use std::process::ExitCode;

use hyprix_gpu::server::{End, Server, warn};
use hyprix_gpu::vulkan::Vulkan;
use hyprix_gpu::wire::Wire;

/// Where an image carries anv's manifest for the helper alone
/// (`cargo xtask`'s `hyprix_gpu`).
const MANIFESTS: &str = "/usr/lib/hyprix-gpu/icd.d";

/// Point the Vulkan loader at the image's own manifests, unless the
/// environment names drivers already (a test's lavapipe).
fn own_drivers() {
    if std::env::var_os("VK_DRIVER_FILES").is_some()
        || std::env::var_os("VK_ICD_FILENAMES").is_some()
        || !std::path::Path::new(MANIFESTS).is_dir()
    {
        return;
    }
    // SAFETY: the program's first act, before any thread is started or
    // anything reads the environment.
    unsafe { std::env::set_var("VK_DRIVER_FILES", MANIFESTS) };
}

/// The render node's device number, major and minor.
fn node(path: &str) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    Some((libc::major(rdev), libc::minor(rdev)))
}

fn main() -> ExitCode {
    own_drivers();
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
