//! The image manifest (`docs/SETTINGS-WRITER.md` §13; ledger 3558 (14),
//! 3636 (14)-(16)): what this build lets a Settings page change.
//!
//! `/etc/ferrix/image-manifest`, written by xtask into every image
//! (`docs/BUILD-FLAGS.md` §5.1): `key=value` lines, `format=1`, then
//! `profile`, `level`, `claim`, `runtime_settings` and the image keys,
//! `settings_writer` among them. Unknown keys are ignored. A missing file,
//! or one whose `format` is not 1, is **unknown**, and unknown is fixed:
//! the page shows its values read-only. Information only: `settingsd`
//! decides nothing from it.

use std::collections::BTreeMap;
use std::path::Path;

/// Where it is.
pub const PATH: &str = "/etc/ferrix/image-manifest";

/// The only format this reads.
const FORMAT: &str = "1";

/// What the image says of itself, or that it says nothing readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Manifest {
    /// No file, or a format this does not read.
    Unknown,
    /// Its keys.
    Known(BTreeMap<String, String>),
}

impl Manifest {
    /// The machine's.
    #[must_use]
    pub fn read() -> Manifest {
        Manifest::read_at(Path::new(PATH))
    }

    /// The manifest at `path`.
    #[must_use]
    pub fn read_at(path: &Path) -> Manifest {
        std::fs::read_to_string(path).map_or(Manifest::Unknown, |text| Manifest::parse(&text))
    }

    /// The manifest `text` holds.
    #[must_use]
    pub fn parse(text: &str) -> Manifest {
        let mut keys = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let _ = keys.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
        if keys.get("format").map(String::as_str) == Some(FORMAT) {
            Manifest::Known(keys)
        } else {
            Manifest::Unknown
        }
    }

    /// The value of `key`, if the manifest is known and says it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        match self {
            Manifest::Unknown => None,
            Manifest::Known(keys) => keys.get(key).map(String::as_str),
        }
    }

    /// Whether the System pages are read-only: the build fixes run-time
    /// settings, carries no settings writer, or does not say.
    #[must_use]
    pub fn fixed(&self) -> bool {
        match self {
            Manifest::Unknown => true,
            Manifest::Known(_) => {
                self.get("runtime_settings") != Some("live")
                    || self.get("settings_writer") != Some("present")
            }
        }
    }

    /// The line a fixed page shows in place of its buttons.
    #[must_use]
    pub fn fixed_text(&self) -> String {
        match self.get("profile") {
            Some(profile) if !profile.is_empty() => {
                format!("Fixed by this build (profile {profile})")
            }
            _ => "Fixed: this build does not say what it allows".to_owned(),
        }
    }

    /// Whether the build carries no settings writer.
    #[must_use]
    pub fn writer_absent(&self) -> bool {
        self.get("settings_writer") == Some("absent")
    }
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    #[test]
    fn a_live_build_with_its_writer_is_not_fixed() {
        let live = Manifest::parse(
            "format=1\nprofile=present\nlevel=present\nclaim=x\nruntime_settings=live\n\
             settings_writer=present\ndriver_updates=absent\nfuture_key=whatever\n",
        );
        assert!(!live.fixed());
        assert_eq!(live.get("profile"), Some("present"));
    }

    #[test]
    fn a_fixed_build_an_absent_writer_or_an_unknown_file_is_fixed() {
        let max = Manifest::parse(
            "format=1\nprofile=max\nruntime_settings=fixed\nsettings_writer=absent\n",
        );
        assert!(max.fixed());
        assert!(max.writer_absent());
        assert_eq!(max.fixed_text(), "Fixed by this build (profile max)");
        let no_writer = Manifest::parse(
            "format=1\nprofile=desktop\nruntime_settings=live\nsettings_writer=absent\n",
        );
        assert!(no_writer.fixed());
        for unknown in [
            "",
            "profile=present\nruntime_settings=live\nsettings_writer=present\n",
            "format=2\nprofile=present\nruntime_settings=live\nsettings_writer=present\n",
        ] {
            let manifest = Manifest::parse(unknown);
            assert_eq!(manifest, Manifest::Unknown, "{unknown:?}");
            assert!(manifest.fixed());
            assert_eq!(
                manifest.fixed_text(),
                "Fixed: this build does not say what it allows"
            );
        }
        assert_eq!(
            Manifest::read_at(std::path::Path::new("/nonexistent/manifest")),
            Manifest::Unknown
        );
    }
}
