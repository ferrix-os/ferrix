//! `notifyd`: the desktop's notifications (`docs/DESKTOP.md` §4.2) --
//! `org.freedesktop.Notifications` on the session bus, and `devmgr`'s
//! driver deaths and restarts -- as banners at the top of the screen.
//!
//! The parts a host test reaches are here: the store, the bus's answers,
//! the bridge's lines and the drawing. `main.rs` joins them to the
//! compositor, the bus and the socket.

pub mod bridge;
pub mod bus;
pub mod paint;
pub mod store;

#[cfg(test)]
mod tests;
