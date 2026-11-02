//! Who at the machine may turn it off (`docs/INIT.md` §10.1): the person of
//! the machine's own seat, as systemd-logind's `allow_active` lets the
//! active local session's user, and nobody else who is not root.
//!
//! A scope is the seat's when root made it and said so (`Call::Scope`'s
//! `seat`): `sessiond`'s graphical session, or `login`'s on the console.
//! A scope a user makes is the seat's when every process it takes is already
//! in a seat's scope of that user's: the programs the desktop starts, each
//! moved into a scope of its own by hyprix. A process that came in some other
//! way -- an ssh session, in `sshd`'s cgroup -- may make itself a scope under
//! its user's slice, and that scope is not the seat's.
//!
//! `Poweroff` and `Reboot` from a uid that is not root are then allowed
//! when the calling process (`SO_PEERCRED`'s pid) is in a seat's scope of
//! that uid, or below one, and `/proc/<pid>/status` still names that uid,
//! so a pid reused by another user's process is not taken for the caller.
//!
//! What this does not claim: a remote session of the same uid that has a
//! program of the seat's session act for it (`hyprctl dispatch exec`) acts
//! as the seat. That is a same-uid boundary, which Ferrix does not draw,
//! as logind and polkit do not.

use std::collections::BTreeMap;

/// The seat's scopes, by unit name, with the uid each belongs to.
#[derive(Debug, Default)]
pub(crate) struct Seats {
    scopes: BTreeMap<String, u32>,
}

/// Where a process is: its cgroup's path from `/proc/<pid>/cgroup`, and
/// its real uid from `/proc/<pid>/status`; `None` for what could not be
/// read, which is never the seat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) cgroup: Option<String>,
    pub(crate) uid: Option<u32>,
}

impl Place {
    /// Where process `pid` is now.
    pub(crate) fn of(pid: u32) -> Place {
        if pid == 0 {
            return Place::default();
        }
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .ok()
            .and_then(|text| cgroup_path(&text).map(str::to_owned));
        let uid = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|text| real_uid(&text));
        Place { cgroup, uid }
    }
}

/// The unified hierarchy's path in `/proc/<pid>/cgroup`'s text: the
/// `0::/path` line.
pub(crate) fn cgroup_path(text: &str) -> Option<&str> {
    text.lines().find_map(|line| line.strip_prefix("0::"))
}

/// The real uid in `/proc/<pid>/status`'s text.
pub(crate) fn real_uid(text: &str) -> Option<u32> {
    let line = text.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The scope a cgroup path is in, under `user-<uid>.slice`, with that uid:
/// `/user.slice/user-1000.slice/session-3.scope/...` is `session-3.scope`
/// of 1000. Nothing for a path anywhere else.
pub(crate) fn user_scope(path: &str) -> Option<(u32, &str)> {
    let mut parts = path.trim_start_matches('/').split('/');
    if parts.next()? != "user.slice" {
        return None;
    }
    let uid = parts
        .next()?
        .strip_prefix("user-")?
        .strip_suffix(".slice")?
        .parse::<u32>()
        .ok()?;
    let scope = parts.next().filter(|name| name.ends_with(".scope"))?;
    Some((uid, scope))
}

impl Seats {
    /// The seat's scope a process is in, below its uid's slice: the uid it
    /// belongs to.
    fn seat_of(&self, place: &Place) -> Option<u32> {
        let (uid, scope) = user_scope(place.cgroup.as_deref()?)?;
        (self.scopes.get(scope) == Some(&uid)).then_some(uid)
    }

    /// Whether the scope `unit` under `slice`, made by `caller` of the
    /// processes at `places`, is the seat's, and whose: root's word (`seat`)
    /// for root's own; for a user's, every process already in a seat's
    /// scope of the same user.
    pub(crate) fn mark(
        &self,
        caller: u32,
        seat: bool,
        slice: Option<&str>,
        places: &[Place],
    ) -> Option<u32> {
        let owner = slice?
            .strip_prefix("user-")?
            .strip_suffix(".slice")?
            .parse::<u32>()
            .ok()?;
        if caller == 0 {
            return seat.then_some(owner);
        }
        let inherited = caller == owner
            && !places.is_empty()
            && places
                .iter()
                .all(|place| place.uid == Some(owner) && self.seat_of(place) == Some(owner));
        inherited.then_some(owner)
    }

    /// Record what scope `unit`, being made now, is: the seat's of a uid, or
    /// not the seat's, whatever an earlier scope of the name was.
    pub(crate) fn record(&mut self, unit: &str, mark: Option<u32>) {
        match mark {
            Some(uid) => {
                let _ = self.scopes.insert(unit.to_owned(), uid);
            }
            None => {
                let _ = self.scopes.remove(unit);
            }
        }
    }

    /// Whether `uid`, calling from a process at `place`, may power the
    /// machine off or restart it.
    pub(crate) fn may_power(&self, uid: u32, place: &Place) -> bool {
        uid == 0 || (place.uid == Some(uid) && self.seat_of(place) == Some(uid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(cgroup: &str, uid: u32) -> Place {
        Place {
            cgroup: Some(cgroup.to_owned()),
            uid: Some(uid),
        }
    }

    const SESSION: &str = "/user.slice/user-1000.slice/session-3.scope";

    /// sessiond's session-3.scope of 1000, marked the seat's by root.
    fn seated() -> Seats {
        let mut seats = Seats::default();
        let mark = seats.mark(
            0,
            true,
            Some("user-1000.slice"),
            &[at("/system.slice/sessiond.service", 0)],
        );
        assert_eq!(mark, Some(1000));
        seats.record("session-3.scope", mark);
        seats
    }

    #[test]
    fn the_seats_person_may_power_off() {
        let seats = seated();
        assert!(seats.may_power(1000, &at(SESSION, 1000)));
        assert!(seats.may_power(1000, &at(&format!("{SESSION}/inner"), 1000)));
        assert!(seats.may_power(0, &Place::default()), "root always");
    }

    #[test]
    fn a_program_the_desktop_started_may_too() {
        let mut seats = seated();
        let mark = seats.mark(1000, false, Some("user-1000.slice"), &[at(SESSION, 1000)]);
        assert_eq!(mark, Some(1000));
        seats.record("app-foot-41.scope", mark);
        let app = at("/user.slice/user-1000.slice/app-foot-41.scope", 1000);
        assert!(seats.may_power(1000, &app));
    }

    #[test]
    fn an_ssh_session_may_not() {
        let mut seats = seated();
        let ssh = at("/system.slice/sshd.service", 1000);
        assert!(!seats.may_power(1000, &ssh));
        // Nor once it has moved itself into a scope of its own.
        let mark = seats.mark(1000, false, Some("user-1000.slice"), &[ssh]);
        assert_eq!(mark, None);
        seats.record("mine.scope", mark);
        assert!(!seats.may_power(1000, &at("/user.slice/user-1000.slice/mine.scope", 1000)));
        // Nor by taking a seat's process along with its own.
        let both = [at(SESSION, 1000), at("/system.slice/sshd.service", 1000)];
        assert_eq!(
            seats.mark(1000, false, Some("user-1000.slice"), &both),
            None
        );
    }

    #[test]
    fn a_user_cannot_say_seat_and_root_must() {
        let seats = seated();
        let ssh = at("/system.slice/sshd.service", 1000);
        assert_eq!(
            seats.mark(1000, true, Some("user-1000.slice"), &[ssh]),
            None
        );
        // Root's scope without the word is not the seat's: a login on a
        // terminal that is not the console.
        assert_eq!(seats.mark(0, false, Some("user-1000.slice"), &[]), None);
        // Nor anything not under a user's slice.
        assert_eq!(seats.mark(0, true, Some("system.slice"), &[]), None);
        assert_eq!(seats.mark(0, true, None, &[]), None);
    }

    #[test]
    fn another_user_may_not() {
        let mut seats = seated();
        // uid 1001 in 1000's session: the place's uid says otherwise.
        assert!(!seats.may_power(1001, &at(SESSION, 1000)));
        // A process of 1001's that reads as being in 1000's session (a pid
        // since reused) is refused for both.
        assert!(!seats.may_power(1001, &at(SESSION, 1001)));
        assert!(!seats.may_power(1000, &at(SESSION, 1001)));
        // 1001 may not make a scope of 1000's processes its own seat.
        assert_eq!(
            seats.mark(1001, false, Some("user-1001.slice"), &[at(SESSION, 1000)]),
            None
        );
        // A scope of the same name under another slice is not the seat's.
        seats.record("x.scope", None);
        assert!(!seats.may_power(
            1001,
            &at("/user.slice/user-1001.slice/session-3.scope", 1001)
        ));
    }

    #[test]
    fn a_scope_made_again_is_judged_again() {
        let mut seats = seated();
        // session-3 ended; a user makes a scope of the same name from ssh.
        let ssh = at("/system.slice/sshd.service", 1000);
        let mark = seats.mark(1000, false, Some("user-1000.slice"), &[ssh]);
        seats.record("session-3.scope", mark);
        assert!(!seats.may_power(1000, &at(SESSION, 1000)));
    }

    #[test]
    fn unread_places_are_never_the_seat() {
        let seats = seated();
        assert!(!seats.may_power(1000, &Place::default()));
        let no_uid = Place {
            cgroup: Some(SESSION.to_owned()),
            uid: None,
        };
        assert!(!seats.may_power(1000, &no_uid));
    }

    #[test]
    fn proc_text_is_read() {
        assert_eq!(
            cgroup_path("0::/user.slice/user-1000.slice/session-3.scope\n"),
            Some(SESSION)
        );
        assert_eq!(
            real_uid("Name:\tsh\nUid:\t1000\t1000\t1000\t1000\n"),
            Some(1000)
        );
        assert_eq!(user_scope(SESSION), Some((1000, "session-3.scope")));
        assert_eq!(user_scope("/user.slice/user-1000.slice"), None);
        assert_eq!(user_scope("/system.slice/a.scope"), None);
        assert_eq!(user_scope("/user.slice/user-x.slice/a.scope"), None);
    }
}
