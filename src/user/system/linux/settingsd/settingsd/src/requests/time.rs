//! `set-timezone` (B3): the zone the C library reads the clock in.
//! `set-time` is B3's later half, parked until every timer the trust chain
//! uses runs on the monotonic clock (ledger 3636 (19); `authd`'s throttle
//! keeps wall-clock milliseconds).
//!
//! * `set-timezone` points `/etc/localtime` at the zone's `TZif` file under
//!   `/usr/share/zoneinfo` (ferrousli, as musl, reads `/etc/localtime` when
//!   `TZ` is unset), and writes the file's POSIX footer to `/etc/TZ`, which
//!   lode reads first (`docs/RUNTIME-SETTINGS.md` §3.2). A zone is a name
//!   that has passed [`ferrix_settings_proto::valid_zone`] -- no `/` at the
//!   start, no `..` -- and its file must resolve inside the zone directory
//!   and start with `TZif`. Both are read from world-readable places, so a
//!   dry run tells nobody anything they could not read themselves.

use std::io;
use std::path::{Path, PathBuf};

use ferrix_settings_proto::{Refusal, RefusalKind};

use super::{Action, Plan, System};
use crate::atomic::{link_atomic, write_atomic};

/// The zones, below the root.
const ZONES: &str = "usr/share/zoneinfo";

/// The link the C library reads, below the root.
const LOCALTIME: &str = "etc/localtime";

/// lode's POSIX string, below the root.
const TZ: &str = "etc/TZ";

/// The zone's file below `root`, if it is a `TZif` file inside the zone
/// directory, and its POSIX footer if it has one.
fn zone_file(root: &Path, zone: &str) -> Result<(PathBuf, Option<String>), Refusal> {
    let dir = root.join(ZONES);
    let missing = || {
        Refusal::new(
            RefusalKind::Invalid,
            format!("there is no time zone called {zone} on this computer"),
        )
    };
    let real_dir = std::fs::canonicalize(&dir).map_err(|_| missing())?;
    let real = std::fs::canonicalize(dir.join(zone)).map_err(|_| missing())?;
    if !real.starts_with(&real_dir) || !real.is_file() {
        return Err(missing());
    }
    let bytes = std::fs::read(&real).map_err(|error| {
        Refusal::new(
            RefusalKind::Failed,
            format!("the zone {zone} cannot be read: {error}"),
        )
    })?;
    if !bytes.starts_with(b"TZif") {
        return Err(missing());
    }
    Ok((Path::new("/").join(ZONES).join(zone), footer(&bytes)))
}

/// A `TZif` file's POSIX footer (RFC 9636 §3.3): the last line, between the
/// last two newlines, in a version 2 or later file.
fn footer(bytes: &[u8]) -> Option<String> {
    if bytes.get(4).copied().unwrap_or(0) < b'2' || bytes.last() != Some(&b'\n') {
        return None;
    }
    let body = bytes.get(..bytes.len() - 1)?;
    let start = body.iter().rposition(|&b| b == b'\n')? + 1;
    let line = std::str::from_utf8(body.get(start..)?).ok()?;
    let plain = !line.is_empty()
        && line.len() <= 64
        && line
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'\\' && b != b'`');
    plain.then(|| line.to_owned())
}

/// The zone `/etc/localtime` names now: its link's target below the zone
/// directory, `UTC` when there is none.
fn zone_now(root: &Path) -> String {
    let Ok(target) = std::fs::read_link(root.join(LOCALTIME)) else {
        return "UTC".to_owned();
    };
    let zones = Path::new("/").join(ZONES);
    target
        .strip_prefix(&zones)
        .ok()
        .and_then(Path::to_str)
        .map_or_else(|| target.display().to_string(), str::to_owned)
}

/// What using `zone` would change.
///
/// # Errors
///
/// [`RefusalKind::Invalid`]: no such zone here.
pub(super) fn plan_zone(zone: &str, system: &System) -> Result<Plan, Refusal> {
    let (target, footer) = zone_file(&system.root, zone)?;
    let before = zone_now(&system.root);
    let changed = before != zone;
    let would = if changed {
        format!("would use the time zone {zone}; it uses {before}")
    } else {
        format!("the time zone is already {zone}")
    };
    Ok(Plan {
        action: Action::Zone {
            zone: zone.to_owned(),
            target,
            footer,
        },
        changed,
        would,
        before,
        after: zone.to_owned(),
    })
}

/// Point `/etc/localtime` at the zone, then write `/etc/TZ`.
///
/// # Errors
///
/// [`RefusalKind::Failed`], saying which of the two was done.
pub(super) fn apply_zone(
    zone: &str,
    target: &Path,
    footer: Option<&str>,
    system: &System,
) -> Result<String, Refusal> {
    link_atomic(&system.root.join(LOCALTIME), target).map_err(|error| {
        Refusal::new(RefusalKind::Failed, format!("nothing was changed: {error}"))
    })?;
    let tz = system.root.join(TZ);
    let written = match footer {
        Some(line) => write_atomic(&tz, format!("{line}\n").as_bytes(), 0o644),
        None => match std::fs::remove_file(&tz) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
    };
    written.map_err(|error| {
        Refusal::new(
            RefusalKind::Failed,
            format!(
                "/etc/localtime names {zone}, but /etc/TZ, which the log reads, was not \
                 written: {error}"
            ),
        )
    })?;
    Ok(format!("the time zone is {zone}"))
}

#[cfg(test)]
mod tests {
    use super::footer;

    #[test]
    fn a_footer_is_the_last_line_of_a_version_2_file() {
        assert_eq!(
            footer(b"TZif2....\nCET-1CEST,M3.5.0,M10.5.0/3\n").as_deref(),
            Some("CET-1CEST,M3.5.0,M10.5.0/3")
        );
        assert_eq!(footer(b"TZif\0....\nUTC0\n"), None);
        assert_eq!(footer(b"TZif2 no newline at the end"), None);
        assert_eq!(footer(b"TZif2....\n\n"), None);
        assert_eq!(footer(b"TZif2....\n`rm`\n"), None);
    }
}
