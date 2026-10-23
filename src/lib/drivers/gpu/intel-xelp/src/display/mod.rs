//! The display engine, as far as the driver goes past reading the plane the
//! firmware lit (`docs/DESKTOP.md` §3.4 and §3.5): what a monitor plugged
//! in or pulled out looks like in the registers, and later the steps that
//! light a port. Pure: registers in, decisions out; the driver process does
//! the reads and writes.

pub mod hotplug;
