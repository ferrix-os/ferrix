//! The sandboxing keys of landing L13 (`docs/INIT.md` §4.5): what a
//! service's processes may not do, set up by the backend in the child
//! between `fork` and `execve`.
//!
//! Five keys are read here: `NoNewPrivileges=`, `PrivateTmp=`,
//! `ProtectSystem=`, `PrivateNetwork=` and `SystemCallFilter=`. Each is
//! parsed as systemd's `load-fragment.c` parses it, and a value that does
//! not parse is a warning in systemd's words, the assignment ignored. What
//! a key means is the backend's: this crate only says what was asked.
//!
//! `SystemCallFilter=` is kept as the list of assignments' words in order,
//! each marked as adding to or taking from the set, because systemd's rule
//! for merging an allow-list with a later deny-list (§4.5) needs the system
//! call groups expanded, and the groups' members are per ABI. The backend
//! expands them when it builds the filter, against the kernel's tables.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::Warnings;
use crate::ini::Assignment;
use crate::keys::{self, Setter};
use crate::value::{self, ValueError};

/// `ProtectSystem=`: which of the system's directories the service sees
/// read-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProtectSystem {
    /// `no`: nothing.
    #[default]
    No,
    /// `yes`: `/usr`, `/boot` and `/efi` (and Ferrix's `/bin`, `/sbin`,
    /// `/lib` and `/lib64`, §4.5).
    Yes,
    /// `full`: as `yes`, and `/etc`.
    Full,
    /// `strict`: everything but `/dev`, `/proc` and `/sys`.
    Strict,
}

/// One word of a `SystemCallFilter=` assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterRule {
    /// A system call's name (`openat`) or a group's (`@mount`).
    pub name: String,
    /// Whether the word adds to the set the filter lists, or takes from it:
    /// systemd's rule, from the assignment's `~` and the first
    /// assignment's.
    pub add: bool,
    /// The action after a `:` (a deny-list's words only): an errno name
    /// (`EPERM`), a number from 1 to 4095, or `kill`.
    pub action: Option<String>,
}

/// `SystemCallFilter=`, merged over its assignments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemCallFilter {
    /// `true` when the first assignment had no `~`: the set lists what is
    /// allowed, and every other call is denied. `false`: the set lists
    /// what is denied.
    pub allow_list: bool,
    /// The words in order. An allow-list starts with `@default`, as
    /// systemd's does.
    pub rules: Vec<FilterRule>,
}

/// The sandboxing keys, as the unit asked for them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sandbox {
    /// `NoNewPrivileges=`.
    pub no_new_privileges: bool,
    /// `PrivateTmp=`: `yes` and `disconnected` alike are a new tmpfs on
    /// `/tmp` and `/var/tmp` (§4.5).
    pub private_tmp: bool,
    /// `ProtectSystem=`.
    pub protect_system: ProtectSystem,
    /// `PrivateNetwork=`.
    pub private_network: bool,
    /// `SystemCallFilter=`; `None` when unset or reset.
    pub system_call_filter: Option<SystemCallFilter>,
}

impl Sandbox {
    /// Whether the unit asked for nothing.
    pub fn is_empty(&self) -> bool {
        *self == Sandbox::default()
    }

    /// Whether the child needs a mount namespace of its own.
    pub fn needs_mount_namespace(&self) -> bool {
        self.private_tmp || self.protect_system != ProtectSystem::No
    }
}

/// systemd's system call groups (`systemd-analyze syscall-filter`, version
/// 259), without the `@`.
pub const GROUPS: [&str; 29] = [
    "default",
    "aio",
    "basic-io",
    "chown",
    "clock",
    "cpu-emulation",
    "debug",
    "file-system",
    "io-event",
    "ipc",
    "keyring",
    "memlock",
    "module",
    "mount",
    "network-io",
    "obsolete",
    "pkey",
    "privileged",
    "process",
    "raw-io",
    "reboot",
    "resources",
    "sandbox",
    "setuid",
    "signal",
    "swap",
    "sync",
    "system-service",
    "timer",
];

/// The keys this module reads.
pub(crate) const KEYS: [(&str, Setter<Sandbox>); 5] = [
    ("NoNewPrivileges", |s, a, w| {
        s.no_new_privileges = keys::boolean(a, w).unwrap_or(s.no_new_privileges);
    }),
    ("PrivateTmp", |s, a, w| {
        let private = keys::parsed(a, w, |text| {
            if text == "disconnected" {
                Ok(true)
            } else {
                value::boolean(text)
            }
        });
        s.private_tmp = private.unwrap_or(s.private_tmp);
    }),
    ("ProtectSystem", |s, a, w| {
        let level = keys::parsed(a, w, |text| {
            Ok::<_, ValueError>(match text {
                "full" => ProtectSystem::Full,
                "strict" => ProtectSystem::Strict,
                other => {
                    if value::boolean(other)? {
                        ProtectSystem::Yes
                    } else {
                        ProtectSystem::No
                    }
                }
            })
        });
        s.protect_system = level.unwrap_or(s.protect_system);
    }),
    ("PrivateNetwork", |s, a, w| {
        s.private_network = keys::boolean(a, w).unwrap_or(s.private_network);
    }),
    ("SystemCallFilter", |s, a, w| {
        system_call_filter(&mut s.system_call_filter, a, w);
    }),
];

/// The sandboxing keys of systemd's that Ferrix has not built. They warn by
/// name and the service runs without them, as §4.4 has it: a unit that
/// loads on systemd loads here.
pub(crate) const NOT_BUILT: [&str; 13] = [
    "ProtectHome",
    "PrivateDevices",
    "PrivateUsers",
    "ProtectKernelTunables",
    "ProtectKernelModules",
    "ProtectControlGroups",
    "RestrictNamespaces",
    "CapabilityBoundingSet",
    "AmbientCapabilities",
    "ReadOnlyPaths",
    "InaccessiblePaths",
    "SystemCallErrorNumber",
    "SystemCallArchitectures",
];

/// The warning for a key of [`NOT_BUILT`].
pub(crate) fn not_built(assignment: &Assignment, warnings: &mut Warnings) {
    warnings.at(
        assignment,
        format!(
            "{}= is not built (landing L13 built NoNewPrivileges=, PrivateTmp=, \
             ProtectSystem=, PrivateNetwork= and SystemCallFilter=); the service runs \
             without it.",
            assignment.key
        ),
    );
}

/// One `SystemCallFilter=` assignment, merged into `filter` as systemd's
/// `config_parse_syscall_filter` merges it: an empty value resets; the
/// first assignment decides whether the set is an allow-list (no `~`, and
/// `@default` is added) or a deny-list (`~`); a later word adds to the set
/// when its assignment agrees with the first, and takes from it otherwise.
fn system_call_filter(
    filter: &mut Option<SystemCallFilter>,
    assignment: &Assignment,
    warnings: &mut Warnings,
) {
    let text = assignment.value.trim();
    if text.is_empty() {
        *filter = None;
        return;
    }
    let (invert, text) = match text.strip_prefix('~') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let filter = filter.get_or_insert_with(|| {
        let mut first = SystemCallFilter {
            allow_list: !invert,
            rules: Vec::new(),
        };
        if !invert {
            first.rules.push(FilterRule {
                name: "@default".to_owned(),
                add: true,
                action: None,
            });
        }
        first
    });
    let add = invert != filter.allow_list;
    for word in text.split([' ', '\t']).filter(|word| !word.is_empty()) {
        let (name, action) = match word.split_once(':') {
            Some((name, action)) => (name, Some(action)),
            None => (word, None),
        };
        if let Some(group) = name.strip_prefix('@') {
            if !GROUPS.contains(&group) {
                warnings.at(
                    assignment,
                    format!("Unknown system call group, ignoring: {name}"),
                );
                continue;
            }
        } else if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            warnings.at(
                assignment,
                format!("Failed to parse system call, ignoring: {name}"),
            );
            continue;
        }
        if let Some(action) = action {
            if !invert {
                warnings.at(
                    assignment,
                    format!(
                        "An allow-listed system call cannot take an error number, ignoring: \
                         {word}"
                    ),
                );
                continue;
            }
            if !is_action(action) {
                warnings.at(
                    assignment,
                    format!("Failed to parse error number, ignoring: {word}"),
                );
                continue;
            }
        }
        filter.rules.push(FilterRule {
            name: name.to_owned(),
            add,
            action: action.map(str::to_owned),
        });
    }
}

/// `SystemCallErrorNumber=`'s format: `kill`, a number from 1 to 4095, or
/// an errno's name. The name is checked against the ABI's table when the
/// filter is built.
fn is_action(text: &str) -> bool {
    if text == "kill" {
        return true;
    }
    if let Ok(number) = text.parse::<u32>() {
        return (1..=4095).contains(&number);
    }
    text.len() > 1
        && text.starts_with('E')
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}
