//! The Linux system call ABI: numbers, `errno` values and structure layouts.
//!
//! Ferrix's native system call interface *is* the Linux one, because the
//! programs it is meant to run — static-musl binaries, ultimately `rustc` —
//! were linked against that interface and cannot be asked to change. This
//! crate is the single written-down copy of it. It is constants, `repr(C)`
//! layouts and pure functions of them -- the number tables, and [`hwcap`]'s
//! reading of identification registers into `AT_HWCAP` -- so it is `no_std`,
//! has no dependencies, forbids `unsafe`, and cannot panic.
//!
//! # Why one crate rather than a module in the kernel
//!
//! Two consumers need these definitions and neither is the other: the kernel's
//! system call entry path, and the host-side tests and fuzzers that feed it
//! recorded arguments. Keeping the ABI in a crate the host toolchain can build
//! is what makes this crate's layout assertions runnable under `cargo test` at
//! all; a wrong `stat` layout is otherwise discovered as a program reading a
//! garbage file size, which is a very long way from its cause.
//!
//! # The two numbering schemes
//!
//! x86-64 inherited its own historical table; AArch64 uses the generic table
//! from `include/uapi/asm-generic/unistd.h`. The same call therefore has two
//! numbers, and some calls exist on only one architecture (AArch64 has no
//! `open`, only `openat`). [`nr::from_x86_64`] and [`nr::from_aarch64`] fold
//! both tables into one [`nr::Syscall`] so the kernel's dispatcher is written
//! once. That translation is the whole point of this crate: the kernel must
//! not carry two dispatch tables.
//!
//! # The return convention
//!
//! A Linux system call returns its result in a single register. A value in
//! `-4095..=-1` is `-errno`; anything else is success. [`errno::encode`] turns
//! a Rust `Result` into that register value, and is the only place the
//! convention is written down.

#![no_std]
#![forbid(unsafe_code)]

pub mod drm;
pub mod errno;
pub mod hwcap;
pub mod inet;
pub mod input;
pub mod kmsg;
pub mod layout;
pub mod netlink;
pub mod nr;
pub mod sigframe32;
pub mod socket;
pub mod sound;
pub mod types;
pub mod user_desc;
pub mod virtgpu;
mod wire;

#[cfg(test)]
mod tests;
