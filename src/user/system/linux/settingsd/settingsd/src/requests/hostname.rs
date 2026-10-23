//! `set-hostname` (B1, B2): `/etc/hostname`, then the running kernel's name.

use std::io;

use ferrix_settings_proto::{Refusal, RefusalKind};

use super::{Action, Plan, System};
use crate::atomic::write_atomic;

/// Where the name is kept, below the root.
const FILE: &str = "etc/hostname";

/// What calling the computer `name` would change. The name has passed
/// [`ferrix_settings_proto::valid_hostname`].
///
/// # Errors
///
/// `/etc/hostname` is there but cannot be read.
pub(super) fn plan(name: &str, system: &System) -> Result<Plan, Refusal> {
    let path = system.root.join(FILE);
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => text.lines().next().unwrap_or_default().trim().to_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(Refusal::new(
                RefusalKind::Failed,
                format!("{} cannot be read: {error}", path.display()),
            ));
        }
    };
    let running = if system.live { live_name() } else { None };
    let changed = before != name || running.is_some_and(|running| running != name);
    let would = if changed {
        format!(
            "would call the computer {name}; it is called {}",
            if before.is_empty() {
                "nothing yet"
            } else {
                &before
            }
        )
    } else {
        format!("the computer is already called {name}")
    };
    Ok(Plan {
        action: Action::Hostname(name.to_owned()),
        changed,
        would,
        before,
        after: name.to_owned(),
    })
}

/// Write `/etc/hostname` and set the running name.
///
/// # Errors
///
/// [`RefusalKind::Failed`]: the file could not be written (nothing
/// changed), or it was and the running name could not be set.
pub(super) fn apply(name: &str, plan: &Plan, system: &System) -> Result<String, Refusal> {
    write_atomic(
        &system.root.join(FILE),
        format!("{name}\n").as_bytes(),
        0o644,
    )
    .map_err(|error| Refusal::new(RefusalKind::Failed, format!("nothing was changed: {error}")))?;
    if system.live {
        // SAFETY: `name` is valid for reads of its length, which is given.
        let set = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
        if set != 0 {
            return Err(Refusal::new(
                RefusalKind::Failed,
                format!(
                    "/etc/hostname says {name}, but the running name could not be set ({}); \
                     it takes effect at the next boot",
                    io::Error::last_os_error()
                ),
            ));
        }
    }
    Ok(if plan.before.is_empty() || plan.before == name {
        format!("the computer is called {name}")
    } else {
        format!("the computer is called {name} (was {})", plan.before)
    })
}

/// The running kernel's name.
fn live_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|text| text.trim().to_owned())
}
