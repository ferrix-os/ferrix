//! The Wayland socket: where a client connects and how descriptors travel.
//!
//! Everything above this crate is bytes and object ids. This is the part that
//! has to be on Ferrix to be tried: an `AF_UNIX` stream socket, `sendmsg` and
//! `recvmsg` with `SCM_RIGHTS`, and the rules about where the socket file
//! goes.
//!
//! # Why not `std::os::unix::net` alone
//!
//! `UnixStream` carries bytes and nothing else. Wayland's `wl_shm.create_pool`
//! and `wl_keyboard.keymap` send descriptors beside the bytes, in a
//! `SCM_RIGHTS` control message, and the standard library's support for those
//! is unstable. So the listening and the accepting are `std`'s, and the two
//! calls that carry a control message are `libc`'s.
//!
//! # A read is a whole number of bytes, not a whole number of messages
//!
//! A stream socket may deliver half a message, and the descriptors that came
//! with a message may arrive before the bytes that name them. [`Connection`]
//! keeps both: the bytes that have not been made into whole messages, and the
//! descriptors that have not been claimed. The server above takes as many
//! whole messages as it can and says how many bytes that was.

mod connection;
mod listener;

pub use connection::{Connection, MAX_QUEUED, RecvError, SendError, Sender};
pub use listener::{Listener, ListenerError, socket_path};

/// The most descriptors one `recvmsg` will be told about.
///
/// libwayland's `MAX_FDS_OUT` is 28 and it never sends more in one message;
/// a client that tried would have its message refused by its own library.
/// Room for twice that means a batch of messages each carrying one is read in
/// a single call.
pub const MAX_FDS_IN: usize = 56;

/// How many bytes one read asks for, matching libwayland's own ring.
pub const BUFFER_BYTES: usize = 4096;

#[cfg(test)]
mod tests;
