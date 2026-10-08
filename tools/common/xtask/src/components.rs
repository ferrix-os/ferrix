//! The parts of the tree that live in repositories of their own.
//!
//! `components.toml` at the root names each one: the path it is checked out
//! at, the repository, and the commit this tree is built and gated with. The
//! path is the one the component had when it was part of this repository, so
//! nothing that reaches it -- a Makefile's `../../ferrousli`, an app's
//! `build.sh`, the code below -- has to know it moved. It is the arrangement
//! seL4's `repo` manifests and Fuchsia's `jiri` have, for the reason they have
//! it: the component's own history and review, and one commit here saying
//! which of its commits everything else was tested against.
//!
//! A component without a `commit` is still in this tree, at its path, and
//! there is nothing to fetch.
//!
//! Every command checks first that each pinned component is there, and
//! clones the missing ones ([`ensure`]), so a fresh clone or worktree builds
//! with no extra step. The clone borrows its objects from a mirror under
//! `~/.local/share/ferrix/components` (or `$FERRIX_COMPONENTS`), which makes
//! a checkout in a new worktree a matter of seconds; on Windows the objects
//! are then copied in, so that git in WSL can read the checkout too. A
//! checkout that is clean and behind its pin is moved to it; one with work
//! of its own -- other commits, or uncommitted changes -- is used as it is
//! and never moved. `cargo xtask components` says which each one is, and
//! `pin-components` writes the commit of every clean checkout that moved
//! past its pin back to the manifest.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::paths::workspace_root;
use crate::{Error, Result};

/// The manifest, at the root of the tree.
pub(crate) const MANIFEST: &str = "components.toml";

/// One component, as the manifest describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Component {
    /// The `[section]` name.
    pub(crate) name: String,
    /// Where it is checked out, relative to the root.
    pub(crate) path: String,
    /// Where it is fetched from.
    pub(crate) repo: String,
    /// The commit this tree is tested against; `None` while it is still part
    /// of this repository.
    pub(crate) commit: Option<String>,
}

/// What a checkout is, against its pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Still part of this repository.
    InTree,
    /// Not checked out.
    Missing,
    /// At the pinned commit.
    AtPin,
    /// At an ancestor of the pin, with nothing of its own uncommitted.
    Behind(String),
    /// Anywhere else, or with uncommitted changes: somebody's work, used as
    /// it is and never moved.
    Own {
        /// Its `HEAD`.
        head: String,
        /// Whether `git status` shows changes.
        dirty: bool,
    },
}

/// Read a manifest's text.
///
/// The format is the subset of TOML the manifest needs: `[name]` sections
/// holding `path`, `repo` and an optional `commit`, each a double-quoted
/// string, and `#` comments. Anything else is an error rather than ignored,
/// so a typo cannot quietly unpin a component.
///
/// # Errors
///
/// On a line outside that subset, a key outside a section, a section
/// without `path` or `repo`, or a name or path given twice.
pub(crate) fn parse(text: &str) -> Result<Vec<Component>> {
    let mut components: Vec<Component> = Vec::new();
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let at = || format!("{MANIFEST}:{}", number + 1);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            let name = name.trim();
            if name.is_empty() || components.iter().any(|known| known.name == name) {
                return Err(Error::new(format!(
                    "{}: `[{name}]` is empty or given twice",
                    at()
                )));
            }
            components.push(Component {
                name: name.to_owned(),
                ..Component::default()
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::new(format!("{}: expected `key = \"value\"`", at())));
        };
        let value = value
            .trim()
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|value| !value.contains('"'))
            .ok_or_else(|| Error::new(format!("{}: the value is not one quoted string", at())))?;
        let Some(component) = components.last_mut() else {
            return Err(Error::new(format!(
                "{}: `{}` comes before any [section]",
                at(),
                key.trim()
            )));
        };
        match key.trim() {
            "path" => value.clone_into(&mut component.path),
            "repo" => value.clone_into(&mut component.repo),
            "commit" => component.commit = Some(value.to_owned()),
            other => return Err(Error::new(format!("{}: unknown key `{other}`", at()))),
        }
    }
    for (index, component) in components.iter().enumerate() {
        if component.path.is_empty() || component.repo.is_empty() {
            return Err(Error::new(format!(
                "{MANIFEST}: [{}] needs both `path` and `repo`",
                component.name
            )));
        }
        if let Some(commit) = &component.commit
            && (commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(Error::new(format!(
                "{MANIFEST}: [{}] commit `{commit}` is not a full 40-digit hash",
                component.name
            )));
        }
        if components
            .iter()
            .take(index)
            .any(|other| other.path == component.path)
        {
            return Err(Error::new(format!(
                "{MANIFEST}: [{}] is checked out where another component is",
                component.name
            )));
        }
    }
    Ok(components)
}

/// The tree's manifest; no components when there is none.
///
/// # Errors
///
/// When it is there and cannot be read or parsed.
pub(crate) fn manifest() -> Result<Vec<Component>> {
    let path = workspace_root().join(MANIFEST);
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(Error::new(format!("{}: {error}", path.display()))),
    }
}

/// Where a component is checked out.
pub(crate) fn checkout(component: &Component) -> PathBuf {
    workspace_root().join(&component.path)
}

/// Bring every pinned component to its pin: clone the missing ones and
/// move the clean ones that are behind it. A checkout with work of its own is
/// left as it is.
///
/// A tree that is not a git checkout -- the copy test-selfhost puts on its
/// volume, for a guest with no network -- carries its components as files
/// and is left as it is.
///
/// # Errors
///
/// When the manifest is unreadable, or a clone or checkout fails.
pub(crate) fn ensure() -> Result<()> {
    if !workspace_root().join(".git").exists() {
        return Ok(());
    }
    sync(&manifest()?)
}

/// `cargo xtask components` and `pin-components`, each after [`ensure`] has
/// brought every checkout it may move to its pin.
///
/// # Errors
///
/// When the manifest cannot be read, or a git command fails.
pub(crate) fn command(command: &str) -> Result<()> {
    let components = manifest()?;
    if components.is_empty() {
        println!("no {MANIFEST}, or no components in it");
        return Ok(());
    }
    if command == "pin-components" {
        pin(&components)?;
    }
    for component in manifest()? {
        println!(
            "{:<12} {:<44} {}",
            component.name,
            component.path,
            describe(&state(&component)?)
        );
    }
    Ok(())
}

/// One line about a state, for `cargo xtask components`.
fn describe(state: &State) -> String {
    match state {
        State::InTree => "in this tree".to_owned(),
        State::Missing => "not checked out".to_owned(),
        State::AtPin => "at the pin".to_owned(),
        State::Behind(head) => format!("behind the pin, at {}", short(head)),
        State::Own { head, dirty } => format!(
            "at {}{}: somebody's work, used as it is",
            short(head),
            if *dirty {
                " with uncommitted changes"
            } else {
                ", not the pin"
            }
        ),
    }
}

/// The first twelve digits of a hash.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// What a component's checkout is, against its pin.
///
/// # Errors
///
/// When git cannot read a checkout that is there.
pub(crate) fn state(component: &Component) -> Result<State> {
    let Some(pin) = &component.commit else {
        return Ok(State::InTree);
    };
    let dir = checkout(component);
    // Its own `.git`, not the directory: a cache restored into the path
    // (CI's `target/`) makes the directory without the checkout, and git run
    // in it would answer for this tree instead.
    if !dir.join(".git").exists() {
        return Ok(State::Missing);
    }
    let head = git(&dir, &["rev-parse", "HEAD"])?;
    let status = git(&dir, &["status", "--porcelain"])?;
    let dirty = !status.is_empty();
    if head == *pin && emptied(&status) {
        return Err(Error::new(format!(
            "{}: the checkout at {} is at its pin with every change a deleted file: a \
             checkout of this tree across the move of {} out of it emptied it. \
             Delete {} and run any xtask command to clone it again.",
            component.name,
            component.path,
            component.name,
            dir.display()
        )));
    }
    if !dirty && head != *pin && !has_commit(&dir, pin) {
        // A pin moved past what this checkout has fetched: fetch, so that a
        // checkout that is only behind is not taken for somebody's work.
        let _ = git(&dir, &["fetch", "--quiet", "origin"])?;
    }
    if !dirty && head == *pin {
        return Ok(State::AtPin);
    }
    if !dirty && is_ancestor(&dir, &head, pin) {
        return Ok(State::Behind(head));
    }
    Ok(State::Own { head, dirty })
}

/// Whether `git status --porcelain` says only that tracked files are gone.
///
/// That is what a checkout of this tree from before a component moved out,
/// and back, leaves: git wrote the component's files into the directory as
/// this tree's, and took them out again. Work of somebody's own is not only
/// deletions.
fn emptied(status: &str) -> bool {
    !status.is_empty()
        && status
            .lines()
            .all(|line| line.trim_start().starts_with("D "))
}

/// Move every checkout that is behind its pin, or missing, to the pin.
fn sync(components: &[Component]) -> Result<()> {
    for component in components {
        forget_interrupted(component)?;
        own_objects(component)?;
        own_line_endings(component)?;
        match state(component)? {
            State::Missing => fetch(component)?,
            State::Behind(_) => {
                let pin = component.commit.as_deref().unwrap_or_default();
                println!("components: moving {} to {}", component.name, short(pin));
                let _ = git(
                    &checkout(component),
                    &["checkout", "--quiet", "--detach", pin],
                )?;
            }
            State::InTree | State::AtPin | State::Own { .. } => {}
        }
    }
    Ok(())
}

/// Write the commit of every clean checkout that is past its pin to the
/// manifest, as long as its repository has that commit: a pin nobody else
/// can fetch would break every other checkout of this tree.
fn pin(components: &[Component]) -> Result<()> {
    let path = workspace_root().join(MANIFEST);
    let mut text = fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    for component in components {
        let State::Own { head, dirty: false } = state(component)? else {
            continue;
        };
        let pin = component.commit.as_deref().unwrap_or_default();
        let dir = checkout(component);
        let _ = git(&dir, &["fetch", "--quiet", "origin"])?;
        if git(&dir, &["branch", "--remotes", "--contains", &head])?.is_empty() {
            return Err(Error::new(format!(
                "{}: {} is on no branch of {}; push it before pinning it",
                component.name,
                short(&head),
                component.repo
            )));
        }
        text = repin(&text, &component.name, pin, &head)?;
        println!(
            "{}: pinned {} (was {})",
            component.name,
            short(&head),
            short(pin)
        );
    }
    fs::write(&path, text).map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

/// The manifest's text with one component's commit replaced, the rest of it
/// -- comments, order, spacing -- as it was.
fn repin(text: &str, name: &str, old: &str, new: &str) -> Result<String> {
    let mut section = String::new();
    let mut done = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(inner) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            inner.trim().clone_into(&mut section);
        }
        if section == name && trimmed.starts_with("commit") && trimmed.contains(old) && !done {
            lines.push(line.replacen(old, new, 1));
            done = true;
        } else {
            lines.push(line.to_owned());
        }
    }
    if !done {
        return Err(Error::new(format!(
            "{MANIFEST}: no commit line in [{name}] to replace"
        )));
    }
    let mut joined = lines.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    Ok(joined)
}

/// Clone a component at its pin, borrowing objects from the mirror.
fn fetch(component: &Component) -> Result<()> {
    let pin = component.commit.as_deref().unwrap_or_default();
    let dir = checkout(component);
    let mirror = mirror(component)?;
    println!(
        "components: cloning {} at {} into {}",
        component.name,
        short(pin),
        component.path
    );
    if dir.exists() {
        // Something is there already -- a restored cache's `target/` -- so
        // make the checkout around it: what `clone --reference` does, by hand.
        let _ = git(&dir, &["init", "--quiet"])?;
        let alternates = dir.join(".git/objects/info/alternates");
        fs::write(
            &alternates,
            format!("{}\n", mirror.join("objects").display()),
        )
        .map_err(|error| Error::new(format!("{}: {error}", alternates.display())))?;
        let _ = git(&dir, &["remote", "add", "origin", &component.repo])?;
        let _ = git(&dir, &["fetch", "--quiet", "origin"])?;
    } else {
        let parent = dir.parent().unwrap_or(&dir);
        fs::create_dir_all(parent)
            .map_err(|error| Error::new(format!("{}: {error}", parent.display())))?;
        let reference = mirror.to_string_lossy().into_owned();
        let target = dir.to_string_lossy().into_owned();
        let _ = git(
            parent,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                "--reference",
                &reference,
                &component.repo,
                &target,
            ],
        )?;
    }
    if !has_commit(&dir, pin) {
        let _ = git(&dir, &["fetch", "--quiet", "origin", pin])?;
    }
    lf_only(&dir)?;
    let _ = git(&dir, &["checkout", "--quiet", "--detach", pin])?;
    own_objects(component)
}

/// On Windows, keep a checkout's files in LF, as they are in its repository.
///
/// This tree's `.gitattributes` says `eol=lf` for everything, but a
/// component is a repository of its own and does not read it, so a Windows
/// git with `core.autocrlf=true` wrote every component file with CRLF. The
/// shell scripts that xtask runs in WSL then stop at their first line
/// (`/usr/bin/env: 'bash\r': No such file or directory`, ferrousli's
/// `tools/build-shared.sh` under `run-compositor --everything`), and
/// `rustfmt` and `check-line-endings.py` disagree with the files.
fn lf_only(dir: &Path) -> Result<()> {
    if cfg!(windows) {
        let _ = git(dir, &["config", "core.autocrlf", "false"])?;
        let _ = git(dir, &["config", "core.eol", "lf"])?;
    }
    Ok(())
}

/// On Windows, rewrite a clean checkout made before [`lf_only`] with LF.
/// One with work of its own keeps its files as they are, and says so.
fn own_line_endings(component: &Component) -> Result<()> {
    if !cfg!(windows) || component.commit.is_none() {
        return Ok(());
    }
    let dir = checkout(component);
    if !dir.join(".git").exists()
        || git(&dir, &["config", "--get", "core.autocrlf"]).is_ok_and(|value| value == "false")
    {
        return Ok(());
    }
    // Asked before the setting changes: with `autocrlf` on, git reads its
    // own CRLF files as clean.
    let clean = git(&dir, &["status", "--porcelain"])?.is_empty();
    lf_only(&dir)?;
    if !clean {
        println!(
            "components: {} has changes of its own, so its files keep the line \
             endings they have; commit or drop them and run any xtask command \
             to have them written with LF",
            component.name
        );
        return Ok(());
    }
    println!(
        "components: writing {}'s files with LF, as its repository has them",
        component.name
    );
    // Git's own recipe for a changed line-ending setting: forget the index,
    // and write every tracked file again from the commit. Nothing is lost,
    // since the checkout had no changes.
    let _ = git(&dir, &["rm", "-r", "-q", "--cached", "."])?;
    let _ = git(&dir, &["reset", "-q", "--hard"])?;
    Ok(())
}

/// On Windows, give a checkout its own copy of the objects it borrows from
/// the mirror, as `git clone --dissociate` does.
///
/// The borrowing is a path in `.git/objects/info/alternates`, and Windows'
/// git writes it as `C:/Users/...`. The same checkout is also read by git
/// in WSL (`/mnt/f/...`) when xtask builds there, and that git cannot
/// resolve a drive-letter path: every command in the checkout then fails
/// with "unable to normalize alternate object path" and "bad object HEAD".
/// A path both can read does not exist when the checkout and the mirror are
/// on different drives, so on Windows the objects are copied instead. A
/// checkout made before this keeps working: the next xtask command copies
/// its objects the same way.
fn own_objects(component: &Component) -> Result<()> {
    if !cfg!(windows) || component.commit.is_none() {
        return Ok(());
    }
    let dir = checkout(component);
    let alternates = dir.join(".git/objects/info/alternates");
    if !alternates.exists() {
        return Ok(());
    }
    println!(
        "components: copying {}'s objects from the mirror into its checkout, \
         so that git in WSL can read it",
        component.name
    );
    // `-a` without `-l` packs what the alternates lend too, so the pack
    // stands on its own once the alternates file is gone.
    let _ = git(&dir, &["repack", "-a", "-d", "-q"])?;
    fs::remove_file(&alternates)
        .map_err(|error| Error::new(format!("{}: {error}", alternates.display())))
}

/// The bare mirror a component's clones borrow from, made or brought up to
/// date with the pin.
fn mirror(component: &Component) -> Result<PathBuf> {
    let root = match std::env::var_os("FERRIX_COMPONENTS") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::home_dir()
            .ok_or_else(|| Error::new("no home directory for the components' mirrors"))?
            .join(".local/share/ferrix/components"),
    };
    let mirror = root.join(format!("{}.git", component.name));
    let pin = component.commit.as_deref().unwrap_or_default();
    if !mirror.exists() {
        fs::create_dir_all(&root)
            .map_err(|error| Error::new(format!("{}: {error}", root.display())))?;
        let target = mirror.to_string_lossy().into_owned();
        let _ = git(
            &root,
            &["clone", "--quiet", "--mirror", &component.repo, &target],
        )?;
    } else if !has_commit(&mirror, pin) {
        let _ = git(&mirror, &["fetch", "--quiet", "--prune", "origin"])?;
    }
    Ok(mirror)
}

/// Take away the `.git` of a clone that was stopped before its checkout,
/// so that it is cloned again.
///
/// A first xtask command stopped by Ctrl-C while it clones leaves a
/// repository with no commit checked out and nothing in its index; every
/// command after it then failed in that checkout ("pathspec '.' did not
/// match any files", "your current branch appears to be broken") until
/// somebody deleted it by hand. With no commit and an empty index there is
/// no work in it to lose: its objects are the mirror's, fetched again.
fn forget_interrupted(component: &Component) -> Result<()> {
    if component.commit.is_none() {
        return Ok(());
    }
    let dir = checkout(component);
    let repository = dir.join(".git");
    if !repository.is_dir()
        || git(&dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
        || !git(&dir, &["ls-files"]).is_ok_and(|files| files.is_empty())
    {
        return Ok(());
    }
    println!(
        "components: {}'s checkout was stopped before it was made; cloning it again",
        component.name
    );
    fs::remove_dir_all(&repository)
        .map_err(|error| Error::new(format!("{}: {error}", repository.display())))
}

/// Whether a repository has a commit.
fn has_commit(dir: &Path, commit: &str) -> bool {
    git(dir, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok()
}

/// Whether `ancestor` is `descendant` or behind it.
fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    has_commit(dir, descendant)
        && Command::new("git")
            .current_dir(dir)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// `git` in `dir`, its output trimmed.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            Error::new(format!("`git {}` would not start: {error}", args.join(" ")))
        })?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "`git {}` in {} failed\n  {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn reads_sections_and_comments() {
        let text = format!(
            "# the libc\n[ferrousli]\npath = \"src/a\"\nrepo = \"https://x/a\"\ncommit = \"{PIN}\"\n\n[zinc]\npath = \"src/b\"\nrepo = \"https://x/b\"\n"
        );
        let components = parse(&text).unwrap();
        assert_eq!(components.len(), 2);
        assert_eq!(components[0].commit.as_deref(), Some(PIN));
        assert_eq!(components[1].commit, None, "no commit: still in this tree");
        assert_eq!(components[1].path, "src/b");
    }

    #[test]
    fn refuses_what_could_quietly_unpin() {
        let base = "[a]\npath = \"p\"\nrepo = \"r\"\n";
        for (bad, why) in [
            (format!("{base}comit = \"{PIN}\"\n"), "unknown key"),
            (format!("{base}commit = {PIN}\n"), "unquoted"),
            (
                format!("{base}commit = \"{}\"\n", PIN.get(..12).unwrap_or_default()),
                "short hash",
            ),
            ("path = \"p\"\n".to_owned(), "key before a section"),
            ("[a]\npath = \"p\"\n".to_owned(), "no repo"),
            (
                format!("{base}[a]\npath = \"q\"\nrepo = \"r\"\n"),
                "name twice",
            ),
            (
                format!("{base}[b]\npath = \"p\"\nrepo = \"r\"\n"),
                "path twice",
            ),
        ] {
            assert!(parse(&bad).is_err(), "{why} was accepted");
        }
    }

    #[test]
    fn repin_changes_only_its_own_section() {
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let text = format!(
            "# keep\n[a]\npath = \"p\"\nrepo = \"r\"\ncommit = \"{PIN}\"\n[b]\npath = \"q\"\nrepo = \"r\"\ncommit = \"{PIN}\"\n"
        );
        let out = repin(&text, "b", PIN, other).unwrap();
        let parsed = parse(&out).unwrap();
        assert_eq!(parsed[0].commit.as_deref(), Some(PIN));
        assert_eq!(parsed[1].commit.as_deref(), Some(other));
        assert!(out.starts_with("# keep\n") && out.ends_with('\n'));
        assert!(repin(&text, "c", PIN, other).is_err());
    }

    #[test]
    fn only_deletions_is_a_checkout_emptied_by_the_move() {
        assert!(emptied(" D Cargo.toml\n D src/main.rs"));
        assert!(
            emptied("D Cargo.toml\n D src/main.rs"),
            "as `git` trimmed it"
        );
        assert!(!emptied(" D Cargo.toml\n M src/main.rs"), "an edit is work");
        assert!(!emptied("?? new.rs"), "a new file is work");
        assert!(!emptied(""), "clean");
    }

    #[test]
    fn the_trees_own_manifest_parses() {
        let components = manifest().unwrap();
        for component in &components {
            assert!(
                !component.path.starts_with('/') && !component.path.contains(".."),
                "[{}] is checked out outside the tree",
                component.name
            );
        }
    }
}
