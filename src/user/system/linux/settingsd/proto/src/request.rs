//! The closed list of what a Settings app may ask for
//! (`docs/SETTINGS-WRITER.md` §4), and each request's own rules.
//!
//! A request is checked against these rules by the client before it is
//! sent, and again by `settingsd` before anyone is asked for a password, so
//! a malformed request costs no prompt and reaches no file.

use core::fmt;

/// The units a Settings page may switch on and off for the next boot:
/// remote login (B1, B6). Nothing else is reachable through
/// `service-enable-persistent`.
pub const SWITCHABLE_UNITS: &[&str] = &["sshd.service", "sshd.socket"];

/// The longest host name, as Linux's `HOST_NAME_MAX`.
const HOST_NAME_MAX: usize = 64;

/// The longest label of a host name (RFC 1123).
const LABEL_MAX: usize = 63;

/// One change to the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Call the computer `name`: `/etc/hostname`, and the running kernel's
    /// name (B1, B2).
    SetHostname {
        /// The new name.
        name: String,
    },
    /// Start `unit` at every boot, or stop doing so: init's `enable` or
    /// `disable` (B1, B6).
    ServiceEnablePersistent {
        /// One of [`SWITCHABLE_UNITS`].
        unit: String,
        /// On or off.
        enabled: bool,
    },
}

/// Which `authd` service decides a request: one for each Settings page's
/// area, so an administrator can loosen one without the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// The computer's name.
    Hostname,
    /// Services started at boot.
    Services,
}

impl Area {
    /// The `authd` service whose policy decides this area.
    #[must_use]
    pub const fn service(self) -> &'static str {
        match self {
            Area::Hostname => "settings.hostname",
            Area::Services => "settings.services",
        }
    }

    /// Its name, as the allow-lists and the prompt name it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Area::Hostname => "hostname",
            Area::Services => "services",
        }
    }
}

/// Why a request breaks its own rules: a line for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Request {
    /// Every request's wire name, in the order of their kinds.
    pub const NAMES: &'static [&'static str] = &["set-hostname", "service-enable-persistent"];

    /// The request's wire name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Request::SetHostname { .. } => "set-hostname",
            Request::ServiceEnablePersistent { .. } => "service-enable-persistent",
        }
    }

    /// The area whose `authd` service decides it.
    #[must_use]
    pub const fn area(&self) -> Area {
        match self {
            Request::SetHostname { .. } => Area::Hostname,
            Request::ServiceEnablePersistent { .. } => Area::Services,
        }
    }

    /// Whether the request keeps its own rules.
    ///
    /// # Errors
    ///
    /// What it breaks, in words for the person who asked.
    pub fn validate(&self) -> Result<(), Invalid> {
        match self {
            Request::SetHostname { name } => valid_hostname(name),
            Request::ServiceEnablePersistent { unit, .. } => {
                if SWITCHABLE_UNITS.contains(&unit.as_str()) {
                    Ok(())
                } else {
                    Err(Invalid(format!(
                        "{} is not a service Settings may switch; it may switch {}",
                        printable(unit),
                        SWITCHABLE_UNITS.join(" and ")
                    )))
                }
            }
        }
    }
}

/// Whether `name` is a host name: 1 to 64 bytes of dot-separated labels,
/// each 1 to 63 ASCII letters, digits and `-`, none starting or ending with
/// `-` (RFC 1123).
///
/// # Errors
///
/// What is wrong with it, in words for a person.
pub fn valid_hostname(name: &str) -> Result<(), Invalid> {
    let shown = printable(name);
    if name.is_empty() {
        return Err(Invalid("a computer's name may not be empty".to_owned()));
    }
    if name.len() > HOST_NAME_MAX {
        return Err(Invalid(format!(
            "a computer's name is at most {HOST_NAME_MAX} characters; {shown} is {}",
            name.len()
        )));
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err(Invalid(format!(
                "{shown}: a name may not start or end with a dot, or have two in a row"
            )));
        }
        if label.len() > LABEL_MAX {
            return Err(Invalid(format!(
                "{shown}: each part between dots is at most {LABEL_MAX} characters"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(Invalid(format!(
                "{shown}: a computer's name may hold only letters a to z, digits, - and ."
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(Invalid(format!(
                "{shown}: no part of a name may start or end with -"
            )));
        }
    }
    Ok(())
}

/// `text` quoted, with anything that is not printable ASCII escaped, so an
/// answer can show what was asked without the asker moving a cursor.
fn printable(text: &str) -> String {
    let short: String = text.chars().take(80).collect();
    format!("{short:?}")
}
