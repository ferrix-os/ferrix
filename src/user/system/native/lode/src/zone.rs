//! The local time zone, found without a time-zone database.
//!
//! In order: the reader's `TZ` (from its environment), `/etc/TZ` (one POSIX
//! string, uClibc's convention), and the POSIX string at the end of a
//! version 2 `/etc/localtime`. Failing those, UTC.

use ferrix_lode_core::time::{Zone, tzif_footer};

use crate::sys;

/// The machine's zone: `/etc/TZ`, then `/etc/localtime`, then UTC.
#[must_use]
pub(crate) fn local() -> Zone {
    if let Some(text) = sys::read_small(b"/etc/TZ")
        && let Some(zone) = core::str::from_utf8(&text).ok().and_then(Zone::posix)
    {
        return zone;
    }
    if let Some(bytes) = sys::read_small(b"/etc/localtime")
        && let Some(zone) = tzif_footer(&bytes).and_then(Zone::posix)
    {
        return zone;
    }
    Zone::utc()
}

/// A reader's zone: its `TZ`, then [`local`].
#[must_use]
pub(crate) fn reader() -> Zone {
    if let Some(environ) = sys::read_small(b"/proc/self/environ") {
        for entry in environ.split(|&byte| byte == 0) {
            if let Some(value) = entry.strip_prefix(b"TZ=")
                && let Some(zone) = core::str::from_utf8(value).ok().and_then(Zone::posix)
            {
                return zone;
            }
        }
    }
    local()
}
