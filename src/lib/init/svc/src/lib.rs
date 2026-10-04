//! The service manager's pure core (`docs/INIT.md` §3).
//!
//! `/sbin/init` is an event loop and a set of backends around this crate. The
//! crate never names a system call and never holds a handle, a descriptor or
//! a directory: the files it reads are bytes a backend handed it, and the
//! things it acts on are names ([`UnitName`] here) that a backend maps to
//! cgroups and processes. That is what lets the same manager run in a
//! Linux-ABI init today and in a native root task later, and it is why the
//! crate is `no_std`: `devmgr`, a `no_std` native program, shares its restart
//! policy.
//!
//! # What is here
//!
//! * [`ini`] -- systemd's INI subset: sections, `Key=value`, comments,
//!   continuation lines, and assignments kept in order so that a repeated
//!   key can add to a list and an empty one can clear it.
//! * [`name`] -- unit names: kinds by suffix, templates and instances, and
//!   the escaping of paths into names.
//! * [`specifier`] -- `%i`, `%n` and the other specifiers a value may carry.
//! * [`value`] -- systemd's value syntaxes: booleans, time spans, sizes,
//!   percentages, signals, and quoted words.
//! * [`exec`] -- `ExecStart=` and its siblings, and `Environment=`.
//! * [`unit`](mod@unit) -- the `[Unit]` and `[Install]` sections: descriptions,
//!   dependencies and conditions.
//! * [`limits`] -- the resource keys of §5.5.
//! * [`kind`] -- the [`Kind`](kind::Kind) trait and the kinds of version 1,
//!   with the service's keys of §4.4.
//! * [`source`] -- the three layered unit directories as something the
//!   caller fills, drop-ins, masking, aliases and templates, and
//!   [`Source::load`](source::Source::load), which puts all of it together.
//! * [`event`] -- what goes into [`Manager::step`] and what comes out, and
//!   the names the backends map: [`UnitId`](event::UnitId),
//!   [`GroupPath`](event::GroupPath), [`Token`](event::Token),
//!   [`ClientId`](event::ClientId).
//! * [`restart`] -- the restart policy of §5.4, shared with `devmgr`.
//! * [`time`] -- the manager's clock, which the backend reads for it.
//! * [`Manager`] -- the dependency graph, transactions and operations, the
//!   slice tree, every kind's state machine, boot and shutdown, as one
//!   state machine: [`Manager::step`] and [`Manager::deadline`].

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

pub mod event;
pub mod exec;
pub mod filter;
pub mod ini;
mod keys;
pub mod kind;
pub mod limits;
mod manager;
pub mod name;
pub mod restart;
pub mod source;
pub mod specifier;
pub mod time;
pub mod unit;
pub mod value;

#[cfg(test)]
mod tests;

pub use manager::{Manager, Mode, NoProbe, Options, Probe};
pub use name::UnitName;
pub use time::Instant;

/// Something wrong with a unit file that does not stop it loading: an
/// unknown key, a value that does not parse, a setting Ferrix does not have
/// yet. systemd logs these and carries on, and so does init, so that a unit
/// written for systemd still loads here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The file, as the directory it was found in and its name there.
    pub file: Arc<str>,
    /// The line, counting from 1; 0 when the warning is about the whole file.
    pub line: u32,
    /// What is wrong, in systemd's words where systemd has words for it.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.file, self.message)
        } else {
            write!(f, "{}:{}: {}", self.file, self.line, self.message)
        }
    }
}

/// The warnings loading one unit produced, in the order it produced them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Warnings {
    list: Vec<Warning>,
}

impl Warnings {
    /// No warnings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one.
    pub fn warn(&mut self, file: &Arc<str>, line: u32, message: String) {
        self.list.push(Warning {
            file: Arc::clone(file),
            line,
            message,
        });
    }

    /// Record one about an assignment, at the line it was written on.
    pub fn at(&mut self, assignment: &ini::Assignment, message: String) {
        self.warn(&assignment.file, assignment.line, message);
    }

    /// Every warning so far.
    pub fn list(&self) -> &[Warning] {
        &self.list
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.list.len()
    }
}
