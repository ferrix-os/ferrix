//! One install, replacement or upgrade of any number of packages as one
//! transaction (`docs/UPDATES.md` §2.3): it is all done, or a failure
//! leaves the root as it was, and a run that stopped part way is put back
//! by the next `pkg` that changes the root ([`recover`]).
//!
//! A btrfs snapshot would be the natural rollback (`docs/APPS.md` §7), but
//! Ferrix's btrfs writer refuses a volume holding one, so `pkg` keeps the
//! old files itself until the new ones are all in place:
//!
//! 1. **Stage.** Every new file is written beside its place, as
//!    `<name>.pkg-new`. Nothing in use has changed; a failure removes them.
//! 2. **Journal.** [`JOURNAL`] lists every path the transaction changes and
//!    whether something was there, under the word `swapping`.
//! 3. **Swap.** Each path that holds something is renamed aside, as
//!    `<name>.pkg-old`, and its new file renamed into its place; a path only
//!    an old version had is renamed aside and nothing put there. The
//!    records come last. A failure here puts every `.pkg-old` back and
//!    removes every new file, from the journal.
//! 4. **Commit.** The journal's word becomes `committed`, and only then are
//!    the `.pkg-old` files deleted and the journal removed.
//!
//! A program that is running keeps the file it started from, which the
//! renames unlink rather than write over.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::{Content, Error, link, under, write_plain};

/// The journal, from the root.
pub const JOURNAL: &str = "var/lib/pkg/transaction";

/// What a staged file's name adds.
pub const NEW: &str = ".pkg-new";

/// What a file set aside adds.
pub const OLD: &str = ".pkg-old";

/// The journal's first line.
const HEADER: &str = "pkg transaction 1";

/// One change of a transaction.
#[derive(Debug, Clone)]
pub(crate) enum Change {
    /// Put this at the path, with this mode.
    Put(String, u32, Content),
    /// Remove what is at the path: a file only an old version had.
    Delete(String),
}

/// What happens to one path, as the journal lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Something was there and a new file takes its place: `r`.
    Replace,
    /// Nothing was there and a new file goes there: `n`.
    New,
    /// Something was there and goes: `d`.
    Delete,
}

impl Step {
    const fn letter(self) -> char {
        match self {
            Step::Replace => 'r',
            Step::New => 'n',
            Step::Delete => 'd',
        }
    }

    fn from_letter(letter: &str) -> Option<Step> {
        match letter {
            "r" => Some(Step::Replace),
            "n" => Some(Step::New),
            "d" => Some(Step::Delete),
            _ => None,
        }
    }
}

/// `path` with `suffix` added to its name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |error| Error(format!("{}: {error}", path.display()))
}

/// Whether anything is at `path`, a dangling link included.
fn there(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Remove `path`'s file if it is there.
fn remove_if_there(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path)(error)),
    }
}

/// Each directory above `path` up to `root` that is empty, removed.
fn prune(root: &Path, path: &Path) {
    let mut at = path.to_path_buf();
    while at.pop() && at != root && fs::remove_dir(&at).is_ok() {}
}

/// Write the journal: `state`, then each step and its path.
fn write_journal(root: &Path, state: &str, steps: &[(Step, String)]) -> Result<(), Error> {
    let path = under(root, JOURNAL);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    let mut text = format!("{HEADER}\n{state}\n");
    for (step, at) in steps {
        text.push(step.letter());
        text.push(' ');
        text.push_str(at);
        text.push('\n');
    }
    let partial = beside(&path, ".partial");
    let mut file = fs::File::create(&partial).map_err(io_error(&partial))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io_error(&partial))?;
    fs::rename(&partial, &path).map_err(io_error(&path))
}

/// A journal: its state, and each step with its path.
type Journal = (String, Vec<(Step, String)>);

/// Read the journal: its state and steps, if there is one.
fn read_journal(root: &Path) -> Result<Option<Journal>, Error> {
    let path = under(root, JOURNAL);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(&path)(error)),
    };
    let mut lines = text.lines();
    let bad = || {
        Error(format!(
            "/{JOURNAL} is not a journal pkg wrote; nothing was done"
        ))
    };
    if lines.next() != Some(HEADER) {
        return Err(bad());
    }
    let state = lines.next().ok_or_else(bad)?.to_owned();
    if state != "swapping" && state != "committed" {
        return Err(bad());
    }
    let mut steps = Vec::new();
    for line in lines {
        let (letter, at) = line.split_once(' ').ok_or_else(bad)?;
        let step = Step::from_letter(letter).ok_or_else(bad)?;
        if !ferrix_cpio::is_safe_path(at) {
            return Err(bad());
        }
        steps.push((step, at.to_owned()));
    }
    Ok(Some((state, steps)))
}

/// Put back every path of `steps` as it was before the swap, from what is
/// there: whatever was set aside goes back, every new file goes.
fn roll_back(root: &Path, steps: &[(Step, String)]) -> Result<(), Error> {
    let mut first_error = None;
    for (step, at) in steps.iter().rev() {
        let target = under(root, at);
        let old = beside(&target, OLD);
        let new = beside(&target, NEW);
        let done = match step {
            Step::Replace | Step::Delete => {
                let back = if there(&old) {
                    fs::rename(&old, &target).map_err(io_error(&target))
                } else {
                    Ok(())
                };
                back.and_then(|()| remove_if_there(&new))
            }
            Step::New => {
                // Staged and not moved, or moved into its place.
                let gone = if there(&new) {
                    remove_if_there(&new)
                } else {
                    remove_if_there(&target)
                };
                prune(root, &target);
                gone
            }
        };
        if let Err(error) = done {
            let _ = first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => remove_if_there(&under(root, JOURNAL)),
    }
}

/// Finish a committed transaction: delete what was set aside, and the
/// directories a deletion emptied, then the journal.
fn finish(root: &Path, steps: &[(Step, String)]) -> Result<(), Error> {
    for (step, at) in steps {
        let target = under(root, at);
        if *step != Step::New {
            remove_if_there(&beside(&target, OLD))?;
        }
        if *step == Step::Delete {
            prune(root, &target);
        }
    }
    remove_if_there(&under(root, JOURNAL))
}

/// What [`recover`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovered {
    /// A transaction stopped before its commit, and was undone.
    RolledBack,
    /// A transaction stopped after its commit, and was finished.
    Finished,
}

/// Put right a transaction that stopped part way, from its journal: undo
/// it if it had not committed, finish it if it had. `None` when there was
/// none.
///
/// # Errors
///
/// A journal that does not read, or a file that could not be put back.
pub fn recover(root: &Path) -> Result<Option<Recovered>, Error> {
    let Some((state, steps)) = read_journal(root)? else {
        return Ok(None);
    };
    if state == "committed" {
        finish(root, &steps)?;
        Ok(Some(Recovered::Finished))
    } else {
        roll_back(root, &steps)?;
        Ok(Some(Recovered::RolledBack))
    }
}

/// Where the tests stop a transaction's swap, after so many renames: dead,
/// as a power cut would, or as a failed rename would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only the tests stop a transaction")
)]
pub(crate) enum Stop {
    /// Return at once, undoing nothing: the next `pkg` must.
    Crash(usize),
    /// Fail as a rename that went wrong does.
    Fail(usize),
}

/// Make `changes` as one transaction, stopped where `stop` says when it is
/// given (the tests').
///
/// # Errors
///
/// A transaction already in the journal, something in the way of a file
/// being set aside, or a file that could not be written or renamed; every
/// change is undone first, and the error says so.
pub(crate) fn run_until(root: &Path, changes: &[Change], stop: Option<Stop>) -> Result<(), Error> {
    if read_journal(root)?.is_some() {
        return Err(Error(format!(
            "/{JOURNAL}: a transaction stopped part way; the next pkg that changes the root puts it right"
        )));
    }
    let mut steps = Vec::new();
    for change in changes {
        let (Change::Put(at, _, _) | Change::Delete(at)) = change;
        if at.contains('\n') {
            return Err(Error(format!("a path with a line break: {at:?}")));
        }
        let was = there(&under(root, at));
        match change {
            Change::Put(..) => {
                steps.push((if was { Step::Replace } else { Step::New }, at.clone()));
            }
            Change::Delete(_) if was => steps.push((Step::Delete, at.clone())),
            Change::Delete(_) => {}
        }
    }
    // Whatever is set aside must be the transaction's own, so that undoing
    // it puts back only what it moved.
    if let Some((_, at)) = steps
        .iter()
        .find(|(step, at)| *step != Step::New && there(&beside(&under(root, at), OLD)))
    {
        return Err(Error(format!(
            "/{at}{OLD} is in the way of /{at} being set aside: refused, nothing changed"
        )));
    }
    // 1. Stage.
    let staged = stage(root, changes);
    if let Err(error) = staged {
        unstage(root, changes);
        return Err(error);
    }
    // 2. Journal.
    if let Err(error) = write_journal(root, "swapping", &steps) {
        unstage(root, changes);
        let _ = remove_if_there(&under(root, JOURNAL));
        return Err(error);
    }
    // 3. Swap.
    let limit = match stop {
        Some(Stop::Crash(after) | Stop::Fail(after)) => Some(after),
        None => None,
    };
    let swapped = swap(root, &steps, limit);
    if let Some(error) = swapped.err() {
        if matches!(stop, Some(Stop::Crash(_))) {
            return Err(error);
        }
        return Err(match roll_back(root, &steps) {
            Ok(()) => Error(format!("{error}; every change undone, nothing changed")),
            Err(again) => Error(format!(
                "{error}; undoing it failed too ({again}): the next pkg that changes the root tries again"
            )),
        });
    }
    // 4. Commit, then delete what was set aside.
    if let Err(error) = write_journal(root, "committed", &steps) {
        return Err(match roll_back(root, &steps) {
            Ok(()) => Error(format!("{error}; every change undone, nothing changed")),
            Err(again) => Error(format!("{error}; undoing it failed too ({again})")),
        });
    }
    finish(root, &steps)
}

/// Write every new file beside its place.
fn stage(root: &Path, changes: &[Change]) -> Result<(), Error> {
    for change in changes {
        let Change::Put(at, mode, content) = change else {
            continue;
        };
        let target = under(root, at);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        let new = beside(&target, NEW);
        remove_if_there(&new)?;
        match content {
            Content::Bytes(bytes) => write_plain(&new, *mode, bytes)?,
            Content::Link(to) => link(to, &new)?,
        }
    }
    Ok(())
}

/// Remove every staged file, and the directories staging made that are
/// empty again.
fn unstage(root: &Path, changes: &[Change]) {
    for change in changes {
        if let Change::Put(at, _, _) = change {
            let target = under(root, at);
            let _ = remove_if_there(&beside(&target, NEW));
            prune(root, &beside(&target, NEW));
        }
    }
}

/// The swap: each step's renames, in order; `stop_after` renames at most.
fn swap(root: &Path, steps: &[(Step, String)], stop_after: Option<usize>) -> Result<(), Error> {
    let mut renames = 0_usize;
    let mut rename = |from: &Path, to: &Path| -> Result<(), Error> {
        if stop_after.is_some_and(|limit| renames >= limit) {
            return Err(Error("stopped".to_owned()));
        }
        renames += 1;
        fs::rename(from, to)
            .map_err(|error| Error(format!("{} to {}: {error}", from.display(), to.display())))
    };
    for (step, at) in steps {
        let target = under(root, at);
        match step {
            Step::Replace => {
                rename(&target, &beside(&target, OLD))?;
                rename(&beside(&target, NEW), &target)?;
            }
            Step::New => rename(&beside(&target, NEW), &target)?,
            Step::Delete => rename(&target, &beside(&target, OLD))?,
        }
    }
    Ok(())
}
