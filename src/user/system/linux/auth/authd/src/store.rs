//! The credential store (`docs/AUTH.md` §5.2): a record per account under
//! `users/`, its failure count and throttle under `state/`, both written
//! only by `authd`, and each replaced whole: a new file beside the old,
//! `fsync`, `rename`, `fsync` of the directory. A crash leaves the old file
//! or the new one, never half of either.
//!
//! ```text
//! format 1
//! account ferrix 1000
//! password $argon2id$v=19$m=19456,t=2,p=1$…$…
//! changed 1790000000
//! ```
//!
//! A record with no `password` line, or with a `locked` line, opens nothing.
//! A record that is not in this format is refused whole, and the account
//! then has no credential until root sets one: guessing at a damaged file
//! is how a store comes to accept what it should not.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use crate::paths::Paths;

/// The only format there is yet.
const FORMAT: &str = "format 1";

/// One account's credential record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credential {
    /// The account it is for.
    pub(crate) account: String,
    /// The uid the account had when the credential was set; checked against
    /// `/etc/passwd` at every use.
    pub(crate) uid: u32,
    /// The stored hash: `$argon2id$…`, or a `$6$`/`$5$` one taken over from
    /// elsewhere and rehashed at its first success.
    pub(crate) password: Option<String>,
    /// Why the account is locked, when it is.
    pub(crate) locked: Option<String>,
    /// When it was last changed, in seconds since 1970.
    pub(crate) changed: u64,
}

impl Credential {
    /// The record as its file holds it.
    pub(crate) fn render(&self) -> String {
        let mut text = format!("{FORMAT}\naccount {} {}\n", self.account, self.uid);
        if let Some(hash) = &self.password {
            text.push_str(&format!("password {hash}\n"));
        }
        if let Some(why) = &self.locked {
            text.push_str(&format!("locked {why}\n"));
        }
        text.push_str(&format!("changed {}\n", self.changed));
        text
    }

    /// Read a record; `None` for anything not in the format.
    pub(crate) fn parse(text: &str) -> Option<Credential> {
        let mut lines = text.lines();
        if lines.next()? != FORMAT {
            return None;
        }
        let (account, uid) = {
            let rest = lines.next()?.strip_prefix("account ")?;
            let (name, uid) = rest.split_once(' ')?;
            (name.to_owned(), uid.parse().ok()?)
        };
        let mut credential = Credential {
            account,
            uid,
            password: None,
            locked: None,
            changed: 0,
        };
        for line in lines {
            let (key, value) = line.split_once(' ')?;
            match key {
                "password" if credential.password.is_none() && !value.is_empty() => {
                    credential.password = Some(value.to_owned());
                }
                "locked" if credential.locked.is_none() => {
                    credential.locked = Some(value.to_owned());
                }
                "changed" => credential.changed = value.parse().ok()?,
                _ => return None,
            }
        }
        Some(credential)
    }
}

/// An account's failures, and until when it is throttled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    /// Consecutive failures since the last success.
    pub(crate) failures: u32,
    /// No attempt is looked at before this, in milliseconds since 1970.
    pub(crate) not_before_ms: u64,
}

impl Tally {
    fn render(self) -> String {
        format!(
            "{FORMAT}\nfailures {}\nnot-before {}\n",
            self.failures, self.not_before_ms
        )
    }

    fn parse(text: &str) -> Option<Tally> {
        let mut lines = text.lines();
        if lines.next()? != FORMAT {
            return None;
        }
        let failures = lines.next()?.strip_prefix("failures ")?.parse().ok()?;
        let not_before_ms = lines.next()?.strip_prefix("not-before ")?.parse().ok()?;
        lines.next().is_none().then_some(Tally {
            failures,
            not_before_ms,
        })
    }
}

/// The store's directories: `auth`'s alone. A program of anyone else's
/// cannot enter them, whatever a record's own mode.
const DIRECTORY_MODE: u32 = 0o700;
/// A record's: `auth`'s alone, the second wall behind the first.
const RECORD_MODE: u32 = 0o600;

/// The store, over its paths.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    paths: Paths,
}

impl Store {
    pub(crate) fn new(paths: Paths) -> Store {
        Store { paths }
    }

    /// Make the store's directories, `0700`, if they are not there.
    ///
    /// # Errors
    ///
    /// The file system's.
    pub(crate) fn prepare(&self) -> io::Result<()> {
        for dir in [
            self.paths.store(),
            self.paths.store().join("users"),
            self.paths.store().join("state"),
        ] {
            fs::create_dir_all(&dir)?;
            fs::set_permissions(&dir, fs::Permissions::from_mode(DIRECTORY_MODE))?;
        }
        if let Some(log_dir) = self.paths.audit().parent() {
            fs::create_dir_all(log_dir)?;
            fs::set_permissions(log_dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// `account`'s record: `Ok(None)` when it has none, `Err` when the file
    /// is there and not a record.
    pub(crate) fn credential(&self, account: &str) -> Result<Option<Credential>, Damaged> {
        match fs::read_to_string(self.paths.user(account)) {
            Ok(text) => Credential::parse(&text)
                .filter(|credential| credential.account == account)
                .map(Some)
                .ok_or(Damaged),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(Damaged),
        }
    }

    /// Replace `account`'s record.
    ///
    /// # Errors
    ///
    /// The file system's; the old record stays when it fails.
    pub(crate) fn set_credential(&self, credential: &Credential) -> io::Result<()> {
        replace(
            &self.paths.user(&credential.account),
            credential.render().as_bytes(),
        )
    }

    /// `account`'s tally; a missing or damaged one is none at all, since the
    /// worst a lost tally costs is a fresh throttle.
    pub(crate) fn tally(&self, account: &str) -> Tally {
        fs::read_to_string(self.paths.state(account))
            .ok()
            .and_then(|text| Tally::parse(&text))
            .unwrap_or_default()
    }

    /// Replace `account`'s tally.
    ///
    /// # Errors
    ///
    /// The file system's.
    pub(crate) fn set_tally(&self, account: &str, tally: Tally) -> io::Result<()> {
        replace(&self.paths.state(account), tally.render().as_bytes())
    }

    /// The seeds an image carries: each file's name is an account and its
    /// one line a hash.
    pub(crate) fn seeds(&self) -> Vec<(String, String)> {
        let Ok(entries) = fs::read_dir(self.paths.seeds()) else {
            return Vec::new();
        };
        let mut seeds: Vec<(String, String)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let text = fs::read_to_string(entry.path()).ok()?;
                let hash = text.lines().next()?.trim().to_owned();
                Some((name, hash))
            })
            .collect();
        seeds.sort();
        seeds
    }
}

/// A record that is there and cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Damaged;

/// Write `bytes` to `path` as a whole new file, `0600`.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let fresh = dir.join(format!(".{}.new", name.to_string_lossy()));
    let _ = fs::remove_file(&fresh);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(RECORD_MODE)
        .open(&fresh)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&fresh, path)?;
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (Store, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("authd-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = Store::new(Paths::under(&root));
        store.prepare().unwrap();
        (store, root)
    }

    #[test]
    fn a_record_round_trips_and_is_0600() {
        let (store, root) = store("round");
        let credential = Credential {
            account: "ferrix".to_owned(),
            uid: 1000,
            password: Some(
                "$argon2id$v=19$m=8,t=1,p=1$c29tZXNhbHQ$aGFzaGhhc2hoYXNoaGFzaA".to_owned(),
            ),
            locked: None,
            changed: 1_790_000_000,
        };
        store.set_credential(&credential).unwrap();
        assert_eq!(store.credential("ferrix"), Ok(Some(credential)));
        let mode = fs::metadata(root.join("var/lib/ferrix/auth/users/ferrix"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(store.credential("nobody"), Ok(None));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_record_is_refused_whole() {
        for bad in [
            "",
            "format 2\naccount a 1\nchanged 0\n",
            "format 1\naccount a one\nchanged 0\n",
            "format 1\naccount a 1\npassword x\npassword y\nchanged 0\n",
            "format 1\naccount a 1\nsomething else\n",
            "format 1\naccount a 1\npassword \nchanged 0\n",
        ] {
            assert_eq!(Credential::parse(bad), None, "{bad:?}");
        }
        let (store, root) = store("damaged");
        fs::write(root.join("var/lib/ferrix/auth/users/a"), "garbage").unwrap();
        assert_eq!(store.credential("a"), Err(Damaged));
        // A record whose account line names another account.
        fs::write(
            root.join("var/lib/ferrix/auth/users/b"),
            "format 1\naccount a 1\nchanged 0\n",
        )
        .unwrap();
        assert_eq!(store.credential("b"), Err(Damaged));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tallies_round_trip_and_default() {
        let (store, root) = store("tally");
        assert_eq!(store.tally("ferrix"), Tally::default());
        let tally = Tally {
            failures: 4,
            not_before_ms: 1_790_000_000_123,
        };
        store.set_tally("ferrix", tally).unwrap();
        assert_eq!(store.tally("ferrix"), tally);
        let _ = fs::remove_dir_all(&root);
    }
}
