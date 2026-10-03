//! Who the accounts are: `/etc/passwd`, read afresh at every use, so an
//! account added or renamed is seen at once (`docs/AUTH.md` §5.2).

use std::path::Path;

/// One `/etc/passwd` line's name, uid, gid and shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    /// The login name.
    pub(crate) name: String,
    /// The uid.
    pub(crate) uid: u32,
    /// The primary gid.
    pub(crate) gid: u32,
    /// The login shell.
    pub(crate) shell: String,
}

/// The first uid of a person's account, as `login.defs`' `UID_MIN`; below
/// it are root and the system's own accounts, `auth` among them.
pub(crate) const PERSON_UID_FIRST: u32 = 1000;

/// The first uid past a person's, as `UID_MAX` plus one; `nobody` and the
/// overflow id (65534) are past it.
pub(crate) const PERSON_UID_END: u32 = 60_001;

impl Account {
    /// Whether it is a person's, who may log in: a uid in the range and a
    /// shell that is not `nologin` or `false`. Root and the system's
    /// accounts are never one (`docs/AUTH.md` §5.4).
    pub(crate) fn is_a_persons(&self) -> bool {
        let shell = self.shell.rsplit('/').next().unwrap_or_default();
        (PERSON_UID_FIRST..PERSON_UID_END).contains(&self.uid)
            && !self.shell.is_empty()
            && !matches!(shell, "nologin" | "false")
    }
}

/// Every account in the file at `path`; none when it cannot be read. A line
/// that is not seven fields with numeric ids is skipped, as musl's `getpwent`
/// skips it.
pub(crate) fn all(path: &Path) -> Vec<Account> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().filter_map(parse).collect()
}

fn parse(line: &str) -> Option<Account> {
    let fields: Vec<&str> = line.split(':').collect();
    if fields.len() != 7 {
        return None;
    }
    let name = *fields.first()?;
    if name.is_empty() || name.starts_with(['+', '-', '#']) {
        return None;
    }
    Some(Account {
        name: name.to_owned(),
        uid: fields.get(2)?.parse().ok()?,
        gid: fields.get(3)?.parse().ok()?,
        shell: (*fields.get(6)?).to_owned(),
    })
}

/// The account called `name`.
pub(crate) fn by_name(path: &Path, name: &str) -> Option<Account> {
    all(path).into_iter().find(|account| account.name == name)
}

/// The first account whose uid is `uid`, as `getpwuid` answers.
pub(crate) fn by_uid(path: &Path, uid: u32) -> Option<Account> {
    all(path).into_iter().find(|account| account.uid == uid)
}

/// Whether `account` is in the group named `group` in the file at `path`:
/// its primary group, or one whose line names it. A missing file or group
/// is no membership (`docs/AUTH.md` §4.2, `TargetGroup=`).
pub(crate) fn in_group(path: &Path, account: &Account, group: &str) -> bool {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    member(&text, account, group)
}

/// [`in_group`] on the file's text.
fn member(text: &str, account: &Account, group: &str) -> bool {
    text.lines().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, gid, members] = fields.as_slice() else {
            return false;
        };
        *name == group
            && (gid.parse::<u32>().is_ok_and(|gid| gid == account.gid)
                || members.split(',').any(|member| member == account.name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_read_as_musl_reads_them() {
        assert_eq!(
            parse("ferrix:x:1000:1000:ferrix:/home/ferrix:/bin/zsh"),
            Some(Account {
                name: "ferrix".to_owned(),
                uid: 1000,
                gid: 1000,
                shell: "/bin/zsh".to_owned(),
            })
        );
        for bad in [
            "",
            "short:x:1",
            "no:x:uid:0:gecos:/:/bin/sh",
            "+nis::0:0:::",
            "a:x:1:1:g:/:/bin/sh:extra",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_group_counts_its_listed_members_and_its_primary_ones() {
        let ferrix = parse("ferrix:x:1000:1000::/home/ferrix:/bin/zsh").unwrap();
        let wheel_by_gid = parse("admin:x:1002:10::/:/bin/sh").unwrap();
        let plain = parse("plain:x:1001:1001::/:/bin/sh").unwrap();
        let group = "root:x:0:\nwheel:x:10:root,ferrix\nferrix:x:1000:\n";
        assert!(member(group, &ferrix, "wheel"));
        assert!(member(group, &wheel_by_gid, "wheel"));
        assert!(!member(group, &plain, "wheel"));
        assert!(!member(group, &ferrix, "audio"));
        assert!(!member("", &ferrix, "wheel"));
        // A name that only contains the account's is not it.
        assert!(!member("wheel:x:10:ferrix2,xferrix\n", &ferrix, "wheel"));
    }

    #[test]
    fn a_persons_account_is_in_the_range_and_has_a_shell() {
        let persons = |line: &str| parse(line).is_some_and(|a| a.is_a_persons());
        assert!(persons("ferrix:x:1000:1000::/home/ferrix:/bin/zsh"));
        assert!(persons("last:x:60000:60000::/:/bin/sh"));
        assert!(!persons("root:x:0:0:root:/:/bin/sh"));
        assert!(!persons(
            "auth:x:90:90:authd:/var/lib/ferrix/auth:/sbin/nologin"
        ));
        assert!(!persons("svc:x:999:999::/:/bin/sh"));
        assert!(!persons("nobody:x:65534:65534::/:/bin/sh"));
        assert!(!persons("locked:x:1001:1001::/:/sbin/nologin"));
        assert!(!persons("locked:x:1001:1001::/:/bin/false"));
        assert!(!persons("empty:x:1001:1001::/:"));
    }
}
