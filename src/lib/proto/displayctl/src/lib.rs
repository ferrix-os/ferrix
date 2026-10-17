//! The display control protocol: what the kernel's display core and a ring-3
//! display driver say to each other over their control channel.
//!
//! `docs/DISPLAY.md` §2.2 is the specification. Frames do not move between
//! the two: the core owns one VMO per buffer, which ATTACH hands the driver
//! to pin read-only as the device's backing, and what crosses the channel is
//! only which VMO is which buffer, which buffer is on which scanout, and
//! which rectangle changed. So there is no ring here,
//! only messages, and a message on the channel is its own doorbell.
//!
//! [`message`] is the bytes: every message a fixed little-endian structure,
//! decoded strictly. [`session`] is the core's half of the conversation as a
//! state machine: it hands out the messages the core sends and judges each
//! reply against what it is waiting for, so a driver that answers a question
//! nobody asked is caught here, host-tested, and not in the kernel.
//!
//! [`edid`] is what a connector's `EDID` property carries: the file
//! `drm.edid_firmware=` names for it, checked, and how the property calls
//! answer for it.
//!
//! [`card`] is who may do what on the card itself: the DRM master's rules,
//! which ioctls only the master may make, and the budget each kind of open
//! may take.
//!
//! Nothing here sends, maps or waits; the glue does.

#![no_std]
#![forbid(unsafe_code)]

pub mod card;
pub mod edid;
pub mod message;
pub mod session;

#[cfg(test)]
mod edid_tests;
#[cfg(test)]
mod tests;
