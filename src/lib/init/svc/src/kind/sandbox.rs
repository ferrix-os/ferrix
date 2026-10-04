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
//! call groups expanded, and the groups' members are per ABI.
//! [`crate::filter::compile`] expands them when it builds the filter. Names
//! are checked here against the tables it uses, so an unknown call, group or
//! errno warns when the unit loads, as systemd's do.

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
    /// The action after a `:` (a deny-list's words only).
    pub action: Option<FilterAction>,
}

/// What a denied call gets: `SystemCallErrorNumber=`'s format, and a
/// deny-list word's after its `:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    /// `kill`: the process ends by `SIGSYS`.
    Kill,
    /// An errno, from 1 to 4095, by number or by name.
    Errno(u32),
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
    /// `SystemCallErrorNumber=`; `None` (unset, empty or `kill`) kills.
    pub system_call_error_number: Option<FilterAction>,
    /// `SystemCallArchitectures=`: `native`, `x86-64`, `x86`, `arm64`,
    /// `arm` and systemd's other names; `None` when unset or reset.
    pub system_call_architectures: Option<Vec<String>>,
}

impl Sandbox {
    /// Whether the unit asked for nothing.
    pub fn is_empty(&self) -> bool {
        *self == Sandbox::default()
    }

    /// Whether the child installs a seccomp filter.
    pub fn filters(&self) -> bool {
        self.system_call_filter.is_some() || self.system_call_architectures.is_some()
    }

    /// Whether the child needs a mount namespace of its own.
    pub fn needs_mount_namespace(&self) -> bool {
        self.private_tmp || self.protect_system != ProtectSystem::No
    }
}

/// The architecture names systemd knows (`ConditionArchitecture=`'s, and
/// `native`). Only those of Ferrix's ABIs change a filter here.
const ARCHITECTURES: [&str; 27] = [
    "native",
    "x86",
    "x86-64",
    "x32",
    "arm",
    "arm-be",
    "arm64",
    "arm64-be",
    "ia64",
    "parisc",
    "parisc64",
    "ppc",
    "ppc-le",
    "ppc64",
    "ppc64-le",
    "s390",
    "s390x",
    "sh",
    "sh64",
    "sparc",
    "sparc64",
    "mips",
    "mips-le",
    "mips64",
    "mips64-le",
    "riscv64",
    "loongarch64",
];

/// The keys this module reads.
pub(crate) const KEYS: [(&str, Setter<Sandbox>); 7] = [
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
    ("SystemCallErrorNumber", |s, a, w| {
        let text = a.value.trim();
        if text.is_empty() {
            s.system_call_error_number = None;
        } else if let Some(action) = action(text) {
            s.system_call_error_number = (action != FilterAction::Kill).then_some(action);
        } else {
            keys::invalid(a, w, "not an errno, its number or kill");
        }
    }),
    ("SystemCallArchitectures", |s, a, w| {
        if a.value.trim().is_empty() {
            s.system_call_architectures = None;
            return;
        }
        let list = s.system_call_architectures.get_or_insert_with(Vec::new);
        for word in a.value.split([' ', '\t']).filter(|word| !word.is_empty()) {
            if !ARCHITECTURES.contains(&word) {
                w.at(a, format!("Failed to parse architecture, ignoring: {word}"));
            } else if !list.iter().any(|known| known == word) {
                list.push(word.to_owned());
            }
        }
    }),
];

/// The sandboxing keys of systemd's that Ferrix has not built. They warn by
/// name and the service runs without them, as §4.4 has it: a unit that
/// loads on systemd loads here.
pub(crate) const NOT_BUILT: [&str; 11] = [
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
];

/// The warning for a key of [`NOT_BUILT`].
pub(crate) fn not_built(assignment: &Assignment, warnings: &mut Warnings) {
    warnings.at(
        assignment,
        format!(
            "{}= is not built (landing L13 built NoNewPrivileges=, PrivateTmp=, \
             ProtectSystem=, PrivateNetwork=, SystemCallFilter=, SystemCallErrorNumber= and \
             SystemCallArchitectures=); the service runs without it.",
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
            if !crate::filter::is_group(group) {
                warnings.at(
                    assignment,
                    format!("Unknown system call group, ignoring: {name}"),
                );
                continue;
            }
        } else if !crate::filter::is_call(name) {
            warnings.at(
                assignment,
                format!("Failed to parse system call, ignoring: {name}"),
            );
            continue;
        }
        let mut parsed = None;
        if let Some(action_text) = action {
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
            let Some(known) = self::action(action_text) else {
                warnings.at(
                    assignment,
                    format!("Failed to parse error number, ignoring: {word}"),
                );
                continue;
            };
            parsed = Some(known);
        }
        filter.rules.push(FilterRule {
            name: name.to_owned(),
            add,
            action: parsed,
        });
    }
}

/// `SystemCallErrorNumber=`'s format: `kill`, a number from 1 to 4095, or
/// an errno's name.
fn action(text: &str) -> Option<FilterAction> {
    if text == "kill" {
        return Some(FilterAction::Kill);
    }
    let number = match text.parse::<u32>() {
        Ok(number) => number,
        Err(_) => crate::filter::errno(text)?,
    };
    (1..=4095)
        .contains(&number)
        .then_some(FilterAction::Errno(number))
}
