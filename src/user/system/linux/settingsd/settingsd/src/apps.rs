//! Which programs may ask for each area (`docs/SETTINGS-WRITER.md` §12.5,
//! S3; ledger 3636 (4)): `/lib/ferrix/settings/apps`, image content, read
//! on every request.
//!
//! ```text
//! # area = program...
//! hostname = /bin/settings /bin/settingsctl
//! ```
//!
//! An entry counts only if it is an absolute path to a file the trusted
//! owner (root) owns, that is neither group- nor world-writable, in
//! directories that are likewise; any other is ignored with a warning. A
//! line that does not parse makes the file allow nothing, as a missing or
//! unreadable one does.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// Where the lists are, below the root.
pub(crate) const FILE: &str = "lib/ferrix/settings/apps";

/// Whether `path` (and every directory above it, below `root`) belongs to
/// `owner` and is writable by nobody else.
fn trusted(root: &Path, path: &str, owner: u32) -> bool {
    if !path.starts_with('/') || path.contains("/../") || path.ends_with("/..") {
        return false;
    }
    let full = root.join(path.trim_start_matches('/'));
    let mut at = Some(full.as_path());
    while let Some(here) = at {
        let Ok(meta) = std::fs::metadata(here) else {
            return false;
        };
        if meta.uid() != owner || meta.mode() & 0o022 != 0 {
            return false;
        }
        if here == root {
            break;
        }
        at = here.parent();
    }
    std::fs::metadata(&full).is_ok_and(|meta| meta.is_file())
}

/// The programs the lists name for `area`, each checked; `None` when the
/// file is missing, unreadable or a line of it does not parse.
pub(crate) fn listed(
    root: &Path,
    owner: u32,
    area: &str,
    warn: &mut dyn FnMut(&str),
) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(root.join(FILE)).ok()?;
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, programs) = line.split_once('=')?;
        let name = name.trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase()) {
            return None;
        }
        if name != area {
            continue;
        }
        for program in programs.split_whitespace() {
            // A program this image does not carry asks nothing, and is no
            // mistake: the lists name every program an image may carry.
            let carried = program.starts_with('/')
                && std::fs::symlink_metadata(root.join(program.trim_start_matches('/'))).is_ok();
            if !carried {
                continue;
            }
            if trusted(root, program, owner) {
                found.push(program.to_owned());
            } else {
                warn(&format!(
                    "settingsd: {FILE}: {program:?} for {area} is not an absolute path to a \
                     root-owned file nobody else may write; it is ignored"
                ));
            }
        }
    }
    Some(found)
}

/// Whether `exe` may ask for `area`.
pub(crate) fn allowed(
    root: &Path,
    owner: u32,
    area: &str,
    exe: &str,
    warn: &mut dyn FnMut(&str),
) -> bool {
    listed(root, owner, area, warn).is_some_and(|programs| programs.iter().any(|p| p == exe))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{FILE, allowed, listed};

    fn root(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("settingsd-apps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["bin", "lib/ferrix/settings", "home"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for dir in [
            "",
            "bin",
            "lib",
            "lib/ferrix",
            "lib/ferrix/settings",
            "home",
        ] {
            std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        for program in ["bin/settings", "bin/settingsctl", "home/mine"] {
            std::fs::write(root.join(program), "").unwrap();
            std::fs::set_permissions(root.join(program), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        root
    }

    fn me() -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    #[test]
    fn only_a_listed_trusted_program_may_ask() {
        let root = root("listed");
        std::fs::write(
            root.join(FILE),
            "# lists\nhostname = /bin/settings /bin/settingsctl\ntime = /bin/settings\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let mut warn = |w: &str| warnings.push(w.to_owned());
        assert!(allowed(
            &root,
            me(),
            "hostname",
            "/bin/settingsctl",
            &mut warn
        ));
        assert!(!allowed(&root, me(), "time", "/bin/settingsctl", &mut warn));
        assert!(!allowed(
            &root,
            me(),
            "services",
            "/bin/settings",
            &mut warn
        ));
        assert!(!allowed(&root, me(), "hostname", "/bin/zinc", &mut warn));
        // Owned by someone else: nothing counts.
        assert!(!allowed(
            &root,
            me() + 1,
            "hostname",
            "/bin/settings",
            &mut warn
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_writable_or_relative_entry_is_ignored_and_a_bad_line_allows_nothing() {
        let root = root("bad");
        std::fs::set_permissions(root.join("home"), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        std::fs::set_permissions(
            root.join("bin/settingsctl"),
            std::fs::Permissions::from_mode(0o775),
        )
        .unwrap();
        std::fs::write(
            root.join(FILE),
            "hostname = /bin/settings /home/mine bin/settings /bin/settingsctl /bin/../bin/settings /bin/none\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let programs = listed(&root, me(), "hostname", &mut |w| {
            warnings.push(w.to_owned());
        });
        assert_eq!(programs, Some(vec!["/bin/settings".to_owned()]));
        // /bin/none is not carried: skipped without a word; bin/settings is
        // not absolute: skipped too.
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        std::fs::write(
            root.join(FILE),
            "hostname = /bin/settings\nthis is no line\n",
        )
        .unwrap();
        assert_eq!(listed(&root, me(), "hostname", &mut |_| {}), None);
        std::fs::write(root.join(FILE), "Hostname = /bin/settings\n").unwrap();
        assert_eq!(listed(&root, me(), "hostname", &mut |_| {}), None);
        std::fs::remove_file(root.join(FILE)).unwrap();
        assert!(!allowed(
            &root,
            me(),
            "hostname",
            "/bin/settings",
            &mut |_| {}
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
