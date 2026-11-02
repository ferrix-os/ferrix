//! What a record's fields are called for a person: one table, kept here
//! so that the viewer's record pane and `lode -o verbose` say the same.
//!
//! The raw key (`_SYSTEMD_UNIT`) stays what lode stores, writes into
//! JSON and filters on; this is only its name on a screen ("Service") and
//! the group it is shown in. A key the table does not know is shown as
//! itself, in [`FieldGroup::Other`].

/// The groups a record's fields are shown in, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldGroup {
    /// What it says: the message, its level and its kind.
    Message,
    /// Who said it: the program, the process, the service, the session.
    Origin,
    /// A crash: the signal and where.
    Crash,
    /// When.
    Time,
    /// Where it is kept: its cursor, its numbers, its boot.
    Storage,
    /// Anything else, under its raw key.
    Other,
}

impl FieldGroup {
    /// The group's heading.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            FieldGroup::Message => "Message",
            FieldGroup::Origin => "Origin",
            FieldGroup::Crash => "Crash",
            FieldGroup::Time => "Time",
            FieldGroup::Storage => "Storage",
            FieldGroup::Other => "Other",
        }
    }
}

/// A field's name for a person, and its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldName {
    /// The key lode stores.
    pub key: &'static str,
    /// What it is called on a screen.
    pub name: &'static str,
    /// Where it is shown.
    pub group: FieldGroup,
}

/// Shorthand for the table.
const fn n(key: &'static str, name: &'static str, group: FieldGroup) -> FieldName {
    FieldName { key, name, group }
}

/// Every key with a name, in the order a record pane lists them within
/// their group.
pub const NAMES: &[FieldName] = &[
    n("MESSAGE", "Message", FieldGroup::Message),
    n("PRIORITY", "Level", FieldGroup::Message),
    n("FERRIX_PRIORITY_SOURCE", "Level from", FieldGroup::Message),
    n("FERRIX_PRIORITY_WORD", "Level word", FieldGroup::Message),
    n("FERRIX_LEVEL", "Level given", FieldGroup::Message),
    n("MESSAGE_ID", "Message type", FieldGroup::Message),
    n("REPEATED", "Repeated", FieldGroup::Message),
    n("HELD_FIRST_SEQNUM", "Held from", FieldGroup::Message),
    n("HELD_LAST_SEQNUM", "Held to", FieldGroup::Message),
    n("SUPPRESSED_SOURCE", "Held source", FieldGroup::Message),
    n("_TRUNCATED", "Cut short", FieldGroup::Message),
    n("SYSLOG_IDENTIFIER", "Program", FieldGroup::Origin),
    n("_COMM", "Process", FieldGroup::Origin),
    n("_PID", "Process ID", FieldGroup::Origin),
    n("SYSLOG_PID", "Process ID said", FieldGroup::Origin),
    n("_EXE", "Program path", FieldGroup::Origin),
    n("_CMDLINE", "Command line", FieldGroup::Origin),
    n("_UID", "User ID", FieldGroup::Origin),
    n("_GID", "Group ID", FieldGroup::Origin),
    n("_SYSTEMD_UNIT", "Service", FieldGroup::Origin),
    n("UNIT", "About service", FieldGroup::Origin),
    n("_SYSTEMD_SLICE", "Slice", FieldGroup::Origin),
    n("_SYSTEMD_CGROUP", "Control group", FieldGroup::Origin),
    n("_SYSTEMD_INVOCATION_ID", "Service run", FieldGroup::Origin),
    n("SESSION_ID", "Session", FieldGroup::Origin),
    n("_SESSION_ID", "Session", FieldGroup::Origin),
    n("SESSION_SEAT", "Seat", FieldGroup::Origin),
    n("SESSION_UID", "Session user", FieldGroup::Origin),
    n("SESSION_SCOPE", "Session scope", FieldGroup::Origin),
    n("SESSION_UNIT", "Session service", FieldGroup::Origin),
    n("_TRANSPORT", "Transport", FieldGroup::Origin),
    n("_STREAM", "Stream", FieldGroup::Origin),
    n("SYSLOG_FACILITY", "Facility", FieldGroup::Origin),
    n("FERRIX_SUBSYSTEM", "Subsystem", FieldGroup::Origin),
    n("_FERRIX_DRIVER", "Driver", FieldGroup::Origin),
    n("_FERRIX_DEVICE", "Device", FieldGroup::Origin),
    n("_HOSTNAME", "Host", FieldGroup::Origin),
    n("_MACHINE_ID", "Machine", FieldGroup::Origin),
    n("CRASH_SIGNAL", "Signal", FieldGroup::Crash),
    n("CRASH_PID", "Crashed process", FieldGroup::Crash),
    n("CRASH_COMM", "Crashed program", FieldGroup::Crash),
    n("CRASH_PROGRAM", "Crashed program path", FieldGroup::Crash),
    n("CRASH_PC", "Code address", FieldGroup::Crash),
    n("CRASH_ADDRESS", "Fault address", FieldGroup::Crash),
    n("CRASH_TRAP", "Trap", FieldGroup::Crash),
    n("MCE_CPU", "Hardware error processor", FieldGroup::Crash),
    n("MCE_BANK", "Hardware error bank", FieldGroup::Crash),
    n("MCE_CLASS", "Hardware error kind", FieldGroup::Crash),
    n("MCE_BANK_TYPE", "Hardware error block", FieldGroup::Crash),
    n("__REALTIME_TIMESTAMP", "Time", FieldGroup::Time),
    n("__MONOTONIC_TIMESTAMP", "Since boot", FieldGroup::Time),
    n("_SOURCE_REALTIME_TIMESTAMP", "Time said", FieldGroup::Time),
    n(
        "_SOURCE_MONOTONIC_TIMESTAMP",
        "Since boot, said",
        FieldGroup::Time,
    ),
    n("__CURSOR", "Cursor", FieldGroup::Storage),
    n("__SEQNUM", "Sequence", FieldGroup::Storage),
    n("_KERNEL_SEQNUM", "Kernel sequence", FieldGroup::Storage),
    n("_BOOT_ID", "Boot", FieldGroup::Storage),
];

/// What `key` is called and where it goes; `None` for a key the table
/// does not know, which is shown as itself in [`FieldGroup::Other`].
#[must_use]
pub fn name_of(key: &str) -> Option<FieldName> {
    NAMES.iter().find(|known| known.key == key).copied()
}

/// Where `key` stands in the table, for sorting a record's fields: known
/// keys in the table's order, the rest after them.
#[must_use]
pub fn order_of(key: &str) -> usize {
    NAMES
        .iter()
        .position(|known| known.key == key)
        .unwrap_or(NAMES.len())
}
