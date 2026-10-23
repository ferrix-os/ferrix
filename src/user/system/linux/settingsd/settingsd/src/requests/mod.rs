//! What each request does (`docs/SETTINGS-WRITER.md` §4, §9): a plan, read
//! from the system, that says what would change -- a dry run's answer and
//! the audit record's before and after -- and the change itself.
//!
//! A new request (B2-B6) is a module here with its own `plan` and `apply`,
//! and an arm in each of [`plan`] and [`apply`].

pub(crate) mod hostname;
pub(crate) mod service;

use std::path::PathBuf;

use ferrix_settings_proto::{Refusal, Request};

pub(crate) use service::{Control, Init};

/// What a request reads and changes.
pub(crate) struct System {
    /// Where `/` is: `/` on the machine, a directory in a test.
    pub(crate) root: PathBuf,
    /// Whether to make the live call too (`sethostname`): never in a test.
    pub(crate) live: bool,
    /// init, for units.
    pub(crate) init: Box<dyn Init>,
}

impl core::fmt::Debug for System {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("System")
            .field("root", &self.root)
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

/// What a request would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The change to make.
    pub(crate) action: Action,
    /// Whether anything would change.
    pub(crate) changed: bool,
    /// What it would do, for a person: a dry run's answer.
    pub(crate) would: String,
    /// The setting before, for the audit record.
    pub(crate) before: String,
    /// The setting after.
    pub(crate) after: String,
}

/// A change, as each request's module makes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// `set-hostname`.
    Hostname(String),
    /// `service-enable-persistent`.
    Service {
        /// The unit.
        unit: String,
        /// On or off.
        enabled: bool,
    },
}

/// Read the system and say what `request` would change.
///
/// # Errors
///
/// A refusal: what is there cannot be read, or rules the request out.
pub(crate) fn plan(request: &Request, system: &System) -> Result<Plan, Refusal> {
    match request {
        Request::SetHostname { name } => hostname::plan(name, system),
        Request::ServiceEnablePersistent { unit, enabled } => Ok(service::plan(unit, *enabled)),
    }
}

/// Make the change `plan` says; what was done, for a person.
///
/// # Errors
///
/// [`ferrix_settings_proto::RefusalKind::Failed`], with what was and was
/// not done.
pub(crate) fn apply(plan: &Plan, system: &System) -> Result<String, Refusal> {
    match &plan.action {
        Action::Hostname(name) => hostname::apply(name, plan, system),
        Action::Service { unit, enabled } => service::apply(unit, *enabled, system),
    }
}
