//! `vtier`: mount the store. See the library's documentation.
//!
//! ```text
//! vtier --backend ocl|ram --gib N --status FILE [--device NAME] MOUNTPOINT
//! ```
//!
//! With `ocl`, the one `OpenCL` device the environment leaves visible must be
//! named NAME (`CUDA_VISIBLE_DEVICES`, by UUID: the card to use is the caller's
//! choice, and this refuses if that left any other).
// AUDIT: as the library's; and a command-line tool's terminal is its interface.
#![cfg_attr(
    target_os = "linux",
    expect(
        unused_results,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::print_stdout,
        clippy::print_stderr,
        reason = "AUDIT: a host cache daemon: a panic ends it, loudly, and costs a cold build"
    )
)]
#![cfg_attr(
    not(target_os = "linux"),
    expect(
        clippy::print_stderr,
        reason = "AUDIT: a CLI tool saying it cannot run here"
    )
)]

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("vtier: FUSE and this card are Linux's: no such slot on this host");
    std::process::ExitCode::FAILURE
}
