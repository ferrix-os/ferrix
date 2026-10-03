//! Where `authd` reads and writes, under one root: `/` on a running system,
//! a directory of its own in a test.

use std::path::{Path, PathBuf};

/// Every path `authd` uses.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    root: PathBuf,
}

impl Paths {
    /// The paths under `root`.
    pub(crate) fn under(root: impl Into<PathBuf>) -> Paths {
        Paths { root: root.into() }
    }

    fn at(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// `/etc/group`: who is in which group (`TargetGroup=`).
    pub(crate) fn group(&self) -> PathBuf {
        self.at("etc/group")
    }

    /// `/etc/passwd`: who the accounts are.
    pub(crate) fn passwd(&self) -> PathBuf {
        self.at("etc/passwd")
    }

    /// The store (`docs/AUTH.md` §5.2).
    pub(crate) fn store(&self) -> PathBuf {
        self.at("var/lib/ferrix/auth")
    }

    /// One account's credential record.
    pub(crate) fn user(&self, account: &str) -> PathBuf {
        self.store().join("users").join(account)
    }

    /// One account's failure count and throttle.
    pub(crate) fn state(&self, account: &str) -> PathBuf {
        self.store().join("state").join(account)
    }

    /// The seeds an image carries (§5.3).
    pub(crate) fn seeds(&self) -> PathBuf {
        self.at("lib/ferrix/auth/seed")
    }

    /// The audit log (§3.6).
    pub(crate) fn audit(&self) -> PathBuf {
        self.at("var/log/ferrix/auth.log")
    }

    /// The three layers of service policy, lowest first (§4.2).
    pub(crate) fn service_layers(&self) -> [PathBuf; 3] {
        [
            self.at("lib/ferrix/auth/services"),
            self.at("etc/ferrix/auth/services"),
            self.at("run/ferrix/auth/services"),
        ]
    }

    /// The root itself.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}
