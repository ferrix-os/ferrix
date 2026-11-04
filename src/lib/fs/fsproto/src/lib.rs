//! The file protocol between `linuxd` and `btrfsd` (`docs/microkernel/BTRFSD.md`
//! §3): what the VFS's `Inode` asks, as channel messages, with the bytes in the
//! request's slot of a VMO the two share.
//!
//! The bytes on a channel are fixed little-endian structures with every length
//! checked here, once, because `linuxd` is QM and a certified server may sit behind
//! this. Everything in the shared VMO can change while it is read: the server
//! copies it out first and checks the copy ([`server`]).
//!
//! One channel is one volume, and a node number is a slot of that channel's table:
//! what one channel names means nothing on another.

#![no_std]

extern crate alloc;

pub mod server;
#[cfg(test)]
mod tests;
pub mod wire;
