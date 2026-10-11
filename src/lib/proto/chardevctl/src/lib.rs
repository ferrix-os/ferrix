//! The chardev control protocol: what the kernel's forwarding core
//! (`src/kernel/src/interfaces/chardev`) and a ring-3 driver whose device
//! files it forwards say to each other over their control channel
//! (`docs/NVIDIA.md` §4.4, N1e).
//!
//! The driver says HELLO once, listing the minors of major 195 it serves,
//! and perhaps its render node ([`node::RENDER_MINOR`]); the kernel answers
//! READY or a named refusal ([`session`]). From then on the kernel writes a
//! REQUEST for each open, ioctl, mmap and release on those nodes, and for
//! the release of each dmabuf the driver made. The driver answers through native calls (`chardev_reply`,
//! `chardev_copy_in`, `chardev_copy_out`), not through the channel, so a
//! reply needs no message of its own.
//!
//! The kernel, not the driver, decides each node's name and mode
//! ([`node`]): a driver names minors, and a minor has one name.
//!
//! Nothing here sends, waits or allocates; the glue does.

#![no_std]
#![forbid(unsafe_code)]

pub mod message;
pub mod node;
pub mod session;

#[cfg(test)]
mod tests;
