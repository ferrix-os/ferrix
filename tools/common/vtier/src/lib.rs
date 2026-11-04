//! vtier: a file system whose blocks live in GPU memory, for a gate slot's
//! target directory (`docs/TEST-TIME.md`, *GPU memory as a gate slot*).
//!
//! A cache and nothing else. A reboot or a driver reset costs a cold build, and
//! a fault must never become a result: every block has a CRC32C over its inode,
//! place and generation, kept in the host and checked on every read, because the
//! card has no ECC, and the first bad block or device error
//! makes the whole store refuse every later operation. `gate.sh` reads the
//! [`store::Health`] from the status file and calls the run an infrastructure
//! failure, whatever xtask said.
//!
//! [`store`] and [`tree`] are pure and tested on the host. The FUSE protocol and
//! `OpenCL` are in the binary, Linux only.
// AUDIT: a host tool holding a cache, not the kernel. A panic here ends the daemon,
// which takes the mount with it: every user gets an error, and the run is an
// infrastructure failure. So a bad index is as loud as a bad block, and the checks
// these lints ask for would only be other ways of saying the same.
#![expect(
    unused_results,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    reason = "AUDIT: a host cache daemon: a panic ends it, loudly, and costs a cold build"
)]

#[cfg(feature = "fault-inject")]
pub mod inject;
pub mod store;
pub mod tree;
pub mod wire;
