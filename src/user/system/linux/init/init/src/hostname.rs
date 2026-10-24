//! `/etc/hostname`: the computer's name, set once `/` is settled
//! (`docs/SETTINGS-WRITER.md` §4, row B2; `docs/INIT.md` §5's
//! `sysinit.target`, "hostname set").
//!
//! The Settings app's Computer name page writes the file through
//! `settingsd`, which also sets the running name; this is what makes the
//! name outlive a restart. The file is read bounded, at most [`READ_MAX`]
//! bytes, and its first line, trimmed, is checked by the very function
//! `settingsd` checks a new name with ([`ferrix_settings_proto::valid_hostname`];
//! the certification consultant's ledger line 3636, condition 18). A file
//! that is missing, empty, too long or holds a name that is not one is
//! said, and the kernel keeps the name it started with: a damaged file
//! never names the machine something no request could have, and never
//! stops the boot.

use std::fs::File;
use std::io::{self, Read as _};

/// Where it is.
const PATH: &str = "/etc/hostname";

/// How much of the file is read: a name, its newline and blanks around it.
const READ_MAX: u64 = 256;

/// What [`from_text`] did, for its line and its tests.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Applied {
    /// The running name is this now.
    Set(String),
    /// The file holds no name, or one that is not one: not set, and why.
    Refused(String),
    /// The call failed.
    Failed(String),
}

/// Set the running name from `text`, the file's first bytes, with `set`
/// making the call.
pub(crate) fn from_text(text: &str, set: &mut dyn FnMut(&str) -> io::Result<()>) -> Applied {
    let name = text.lines().next().unwrap_or_default().trim();
    if name.is_empty() {
        return Applied::Refused("its first line is empty".to_owned());
    }
    if let Err(invalid) = ferrix_settings_proto::valid_hostname(name) {
        return Applied::Refused(invalid.0);
    }
    match set(name) {
        Ok(()) => Applied::Set(name.to_owned()),
        Err(error) => Applied::Failed(format!("sethostname({name}): {error}")),
    }
}

/// `sethostname(2)`.
fn sethostname(name: &str) -> io::Result<()> {
    // SAFETY: `name` is valid for reads of its length, which is given.
    let status = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The file's first [`READ_MAX`] bytes, as text.
fn read_bounded() -> io::Result<String> {
    let mut bytes = Vec::new();
    let _ = File::open(PATH)?.take(READ_MAX).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Set the running name from `/etc/hostname`, saying what was done.
pub(crate) fn apply(say: &dyn Fn(&str)) {
    let text = match read_bounded() {
        Ok(text) => text,
        Err(error) => {
            say(&format!(
                "the computer keeps its default name: {PATH}: {error}"
            ));
            return;
        }
    };
    match from_text(&text, &mut sethostname) {
        Applied::Set(name) => say(&format!("the computer is called {name} ({PATH})")),
        Applied::Refused(why) => say(&format!(
            "the computer keeps its default name: {PATH}: {why}"
        )),
        Applied::Failed(why) => say(&format!("the computer keeps its default name: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{Applied, from_text};

    #[test]
    fn the_first_line_is_the_name_if_settingsd_would_take_it() {
        let mut calls = Vec::new();
        let mut set = |name: &str| {
            calls.push(name.to_owned());
            Ok(())
        };
        assert_eq!(
            from_text("  box \nother\n", &mut set),
            Applied::Set("box".to_owned())
        );
        for bad in [
            "",
            "\nbox\n",
            "rm -rf\n",
            "-box\n",
            "a..b\n",
            "bøx\n",
            &format!("{}\n", "a".repeat(65)),
        ] {
            assert!(
                matches!(from_text(bad, &mut set), Applied::Refused(_)),
                "{bad:?}"
            );
        }
        // A line longer than the read: cut at 256 bytes, and still no name.
        let long = "a".repeat(300);
        let read = long
            .get(..usize::try_from(super::READ_MAX).unwrap())
            .unwrap();
        assert!(matches!(from_text(read, &mut set), Applied::Refused(_)));
        assert_eq!(calls, vec!["box".to_owned()]);
        let mut refusing = |_: &str| Err(std::io::Error::from_raw_os_error(libc::EPERM));
        assert!(matches!(
            from_text("box", &mut refusing),
            Applied::Failed(why) if why.starts_with("sethostname(box): ")
        ));
    }
}
