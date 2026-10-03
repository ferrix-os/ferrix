//! The kernel's judgement of a driver's HELLO.

use crate::message::{Hello, MAX_NODES, VERSION};
use crate::node;

/// Why a HELLO was refused, as REFUSED names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The first message was not a HELLO, or did not decode.
    Protocol,
    /// It names a version this kernel does not speak.
    Version,
    /// It names another device than the one the control was made for.
    Location,
    /// It lists a minor no name is given for ([`node::name`]).
    Minor,
    /// It lists one minor twice.
    Duplicate,
    /// Another control already serves one of its minors.
    Taken,
    /// The kernel had no memory to publish the nodes.
    NoMemory,
}

impl Refusal {
    /// Its byte in REFUSED.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Refusal::Protocol => 1,
            Refusal::Version => 2,
            Refusal::Location => 3,
            Refusal::Minor => 4,
            Refusal::Duplicate => 5,
            Refusal::Taken => 6,
            Refusal::NoMemory => 7,
        }
    }

    /// The refusal a byte names.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Refusal> {
        match code {
            1 => Some(Refusal::Protocol),
            2 => Some(Refusal::Version),
            3 => Some(Refusal::Location),
            4 => Some(Refusal::Minor),
            5 => Some(Refusal::Duplicate),
            6 => Some(Refusal::Taken),
            7 => Some(Refusal::NoMemory),
            _ => None,
        }
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Refusal::Protocol => "its first message was not a HELLO",
            Refusal::Version => "its HELLO names another protocol version",
            Refusal::Location => "its HELLO names another device",
            Refusal::Minor => "its HELLO lists a minor with no name",
            Refusal::Duplicate => "its HELLO lists a minor twice",
            Refusal::Taken => "another driver already serves one of its minors",
            Refusal::NoMemory => "there was no memory to publish its nodes",
        })
    }
}

/// What a HELLO the kernel took publishes: the minors, in the order listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Publication {
    /// How many of `minors` there are.
    pub count: usize,
    /// The minors.
    pub minors: [u16; MAX_NODES],
}

impl Publication {
    /// The minors.
    #[must_use]
    pub fn minors(&self) -> &[u16] {
        self.minors.get(..self.count).unwrap_or(&[])
    }
}

/// Judge `hello` from the driver of the device at `location`. Whether
/// another control serves a minor is the caller's to ask, under its lock.
///
/// # Errors
///
/// The [`Refusal`] the kernel answers with.
pub fn judge(hello: &Hello, location: u32) -> Result<Publication, Refusal> {
    if hello.version != VERSION {
        return Err(Refusal::Version);
    }
    if hello.location != location {
        return Err(Refusal::Location);
    }
    let minors = hello.minors.get(..hello.count).ok_or(Refusal::Protocol)?;
    if minors.is_empty() {
        return Err(Refusal::Protocol);
    }
    for (index, minor) in minors.iter().enumerate() {
        if node::name(*minor).is_none() {
            return Err(Refusal::Minor);
        }
        if minors.iter().skip(index + 1).any(|other| other == minor) {
            return Err(Refusal::Duplicate);
        }
    }
    Ok(Publication {
        count: hello.count,
        minors: hello.minors,
    })
}
