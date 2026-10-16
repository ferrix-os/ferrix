//! oh-my-zsh, the configuration every shell in the image starts with.
//!
//! zinc's acceptance criterion is that oh-my-zsh runs in it, and a criterion
//! nothing boots with is one nobody meets twice. The image therefore carries a
//! checkout of oh-my-zsh at [`DIRECTORY`] and an `/etc/zshrc` that sources it,
//! so the shell the kernel starts comes up with the theme, the aliases and the
//! completion oh-my-zsh gives it, rather than as a bare shell someone must
//! first install something into over a network the guest may not have.
//!
//! The checkout is a download rather than something in this repository, for
//! the reason busybox and the ports are: it is another project's tree, under
//! another project's licence, changing on their schedule and not on ours. It
//! is installed under `~/.local/share/ferrix/oh-my-zsh` (or `$FERRIX_OMZ`) by
//! `cargo xtask omz --from <DIRECTORY-OR-URL>`, the one thing here that
//! reaches the network, and then only because it was asked for by name. An
//! image built on a machine without it says so and boots without it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "omz",
        help_at: 0,
        help: &[],
        run: |_, args| install(args.from.as_deref()),
    },
    Task {
        name: "zsh-functions",
        help_at: 0,
        help: &[],
        run: |_, args| install_functions(args.from.as_deref()),
    },
];

/// Where zsh's function tree goes in the image: the directory zinc's default
/// `fpath` is made of. oh-my-zsh calls `compinit`, `is-at-least`,
/// `add-zsh-hook`, `colors` and the rest as zsh gives them, autoloaded from
/// this tree; without it every one of those is `command not found`.
pub(crate) const FUNCTIONS: &str = "usr/share/zsh/functions";

/// Where the checkout goes in the image. Shared rather than in a home
/// directory, because root and `ferrix` start the same shell and the tree is
/// megabytes of the guest's memory, which it should be once.
pub(crate) const DIRECTORY: &str = "usr/share/oh-my-zsh";

/// What an image carrying the checkout writes to `/etc/zshrc`, which zinc
/// sources before a user's own `~/.zshrc`, and only for an interactive shell.
///
/// `$ZSH` is where the tree is rather than the `$HOME/.oh-my-zsh` an install
/// would have made, and everything oh-my-zsh writes as it starts -- its
/// cache, its completion dump -- goes to `/tmp`, because the home of the user
/// the kernel starts a shell as is `/`, and a start-up that writes into the
/// root of the filesystem is not one to have on every boot.
pub(crate) const ZSHRC: &[u8] = b"\
# Ferrix starts its shells with oh-my-zsh, from the copy the image carries.
# A user's own ~/.zshrc is sourced after this one and can undo any of it.
export ZSH=/usr/share/oh-my-zsh
export HOME=${HOME:-/}
# agnoster, as the build machine's own shell has it, where the terminal can
# draw its Powerline separators; robbyrussell on the console (TERM=dumb),
# whose font and whose serial line's other end may have no such glyphs.
if [[ $TERM == dumb ]]; then
  ZSH_THEME=${ZSH_THEME:-robbyrussell}
else
  ZSH_THEME=${ZSH_THEME:-agnoster}
fi
plugins=(git)
# No compaudit. oh-my-zsh runs it on every start, once in a process of its
# own in the background and once more in compinit, to warn of a directory
# of fpath that someone other than root and the user could write to. Every
# one here is the image's, root's and 0755, or the /tmp one this file makes
# for the user, so it warns of nothing, at the price of a stat of every
# completion function and a process beside the desktop at every start.
ZSH_DISABLE_COMPFIX=true
# Where oh-my-zsh writes: its cache and its completion dump. /tmp, because it
# is the one directory every boot has and can write in.
export ZSH_CACHE_DIR=/tmp/oh-my-zsh
export ZSH_COMPDUMP=/tmp/oh-my-zsh/zcompdump
mkdir -p $ZSH_CACHE_DIR/completions
[[ -r $ZSH/oh-my-zsh.sh ]] && source $ZSH/oh-my-zsh.sh
";

/// Names in the checkout that are the project's own machinery rather than
/// anything a shell sources: its history, how it is developed, and the cache
/// a run of it on the build host left behind.
const SKIPPED: &[&str] = &[".git", ".github", ".devcontainer", "cache"];

/// What a file is for when its name ends in one of these: a person reading
/// about a plugin -- its README, its screenshots, the animated demo -- and
/// never a shell starting one. 3.8 MB of oh-my-zsh's 7.5 MB (2026-09-24), in
/// every image that carries it and on a board's card, where the loader reads
/// the whole archive at some 16 MB/s on every boot.
const READ_BY_PEOPLE: &[&str] = &[".md", ".gif", ".png", ".jpg"];

/// Whether a name in a tree is left out of the image: [`SKIPPED`], the
/// `.zwc` files zsh compiles its functions into, which are zsh's own binary
/// format rather than anything zinc reads -- it finds the plain file beside
/// each one -- and what is [`READ_BY_PEOPLE`].
fn skipped(name: &str) -> bool {
    SKIPPED.contains(&name)
        || name.ends_with(".zwc")
        || READ_BY_PEOPLE.iter().any(|suffix| name.ends_with(suffix))
}

/// Where the checkout is installed on this machine.
fn root() -> Result<PathBuf> {
    installed_under("FERRIX_OMZ", "oh-my-zsh")
}

/// Where zsh's function tree is installed on this machine.
fn functions_root() -> Result<PathBuf> {
    installed_under("FERRIX_ZSH_FUNCTIONS", "zsh-functions")
}

/// `$variable` when it is set, `~/.local/share/ferrix/<name>` otherwise.
fn installed_under(variable: &str, name: &str) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(variable) {
        return Ok(PathBuf::from(dir));
    }
    std::env::home_dir()
        .map(|home| {
            [".local", "share", "ferrix", name]
                .iter()
                .fold(home, |dir, name| dir.join(name))
        })
        .ok_or_else(|| {
            Error::new(format!(
                "no home directory to find {name} under; set {variable}"
            ))
        })
}

/// The permissions a file in the checkout is given: executable when it starts
/// as a script, 0644 otherwise, rather than read off the build host, so that
/// a checkout copied to a filesystem without them gives the same archive.
fn mode_of(bytes: &[u8]) -> u32 {
    if bytes.starts_with(b"#!") {
        0o755
    } else {
        0o644
    }
}

/// Read the tree at `path` into `out` as the archive path `name`, in name
/// order, leaving out what is [`skipped`].
fn read_tree(path: &Path, name: &str, out: &mut Vec<File>) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
        out.push(File {
            path: name.to_owned(),
            mode: 0o777,
            content: Content::Link(target.to_string_lossy().replace('\\', "/")),
        });
    } else if meta.is_dir() {
        out.push(File {
            path: name.to_owned(),
            mode: 0o755,
            content: Content::Directory,
        });
        let mut children: Vec<String> = std::fs::read_dir(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<_>>()?;
        children.sort();
        for child in children {
            if skipped(&child) {
                continue;
            }
            read_tree(&path.join(&child), &format!("{name}/{child}"), out)?;
        }
    } else {
        let bytes = std::fs::read(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
        out.push(File {
            path: name.to_owned(),
            mode: mode_of(&bytes),
            content: Content::Bytes(bytes),
        });
    }
    Ok(())
}

/// What an image carrying `zinc` carries beside it, and nothing at all when
/// there is no shell in it to configure.
pub(crate) fn beside(zinc: Option<&[u8]>) -> Result<Vec<File>> {
    if zinc.is_none() {
        return Ok(Vec::new());
    }
    installed()
}

/// The checkout for an image to carry, with the `/etc/zshrc` that sources it,
/// or nothing at all when it is not installed on this machine.
fn installed() -> Result<Vec<File>> {
    let root = root()?;
    if !root.join("oh-my-zsh.sh").is_file() {
        println!(
            "  oh-my-zsh is not installed under {}, so not in the image \
             (cargo xtask omz --from <DIRECTORY-OR-URL> installs it)",
            root.display()
        );
        return Ok(Vec::new());
    }
    let start = root.join("oh-my-zsh.sh");
    let text = std::fs::read(&start)
        .map_err(|error| Error::new(format!("reading {}: {error}", start.display())))?;
    if text.windows(2).any(|pair| pair == b"\r\n") {
        return Err(Error::new(format!(
            "{} has CRLF line endings, which zsh cannot parse -- git's core.autocrlf \
             checked it out that way; install it again with cargo xtask omz --from <URL>, \
             which clones with LF",
            start.display()
        )));
    }
    let mut files = vec![File {
        path: "etc/zshrc".to_owned(),
        mode: 0o644,
        content: Content::Bytes(ZSHRC.to_vec()),
    }];
    read_tree(&root, DIRECTORY, &mut files)?;
    let functions = functions_root()?;
    if functions.join("Misc").join("is-at-least").is_file() {
        read_tree(&functions, FUNCTIONS, &mut files)?;
    } else {
        println!(
            "  zsh's functions are not installed under {}, so oh-my-zsh will              find no compinit (cargo xtask zsh-functions --from <DIRECTORY>              installs them)",
            functions.display()
        );
    }
    Ok(files)
}

/// `cargo xtask omz --from <DIRECTORY-OR-URL>`: install the checkout every
/// image carries, from a directory on this machine or from a git repository
/// to clone.
pub(crate) fn install(from: Option<&str>) -> Result<()> {
    let from = from.ok_or_else(|| {
        Error::new(
            "say where oh-my-zsh comes from: --from <DIRECTORY>, a checkout on this machine, \
             or --from https://github.com/ohmyzsh/ohmyzsh.git to clone one",
        )
    })?;
    let root = root()?;
    let source = Path::new(from);
    if source.exists() && !source.join("oh-my-zsh.sh").is_file() {
        return Err(Error::new(format!(
            "{from} is not an oh-my-zsh checkout: it has no oh-my-zsh.sh"
        )));
    }
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .map_err(|error| Error::new(format!("removing {}: {error}", root.display())))?;
    }
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::new(format!("making {}: {error}", parent.display())))?;
    }
    if source.exists() {
        copy_tree(source, &root)?;
    } else {
        // LF whatever the host's git says: a Windows git with core.autocrlf
        // checks the scripts out with CRLF, and zsh reads each \r as part of
        // the line, so oh-my-zsh.sh fails to parse in the guest.
        let mut command = Command::new("git");
        let _ = command
            .args(["-c", "core.autocrlf=false", "-c", "core.eol=lf"])
            .args([
                "clone",
                "--depth",
                "1",
                "--config",
                "core.autocrlf=false",
                from,
            ])
            .arg(&root);
        crate::cargo::run(command, "git clone (oh-my-zsh)")?;
    }
    println!("\ninstalled oh-my-zsh under {}", root.display());
    Ok(())
}

/// `cargo xtask zsh-functions --from <DIRECTORY>`: install zsh's function
/// tree, which every image carrying oh-my-zsh carries beside it at
/// [`FUNCTIONS`]. The directory is either an installed tree, such as a Linux
/// machine's `/usr/share/zsh/functions`, or a zsh source checkout, of which
/// only `Completion` and `Functions` are taken. A directory rather than a
/// download, because any machine with zsh already has one.
pub(crate) fn install_functions(from: Option<&str>) -> Result<()> {
    let from = from.ok_or_else(|| {
        Error::new(
            "say where zsh's functions come from: --from <DIRECTORY>, such as              /usr/share/zsh/functions or a zsh source checkout",
        )
    })?;
    let source = Path::new(from);
    let installed = source.join("Misc").join("is-at-least").is_file();
    let checkout = source
        .join("Functions")
        .join("Misc")
        .join("is-at-least")
        .is_file()
        && source.join("Completion").is_dir();
    if !installed && !checkout {
        return Err(Error::new(format!(
            "{from} is neither zsh's function tree nor a zsh source checkout:              it has no Misc/is-at-least"
        )));
    }
    let root = functions_root()?;
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .map_err(|error| Error::new(format!("removing {}: {error}", root.display())))?;
    }
    if installed {
        copy_tree(source, &root)?;
    } else {
        for part in ["Completion", "Functions"] {
            copy_tree(&source.join(part), &root.join(part))?;
        }
    }
    println!(
        "
installed zsh's functions under {}",
        root.display()
    );
    Ok(())
}

/// Copy the tree at `from` to `to`, [`skipped`] names apart.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .map_err(|error| Error::new(format!("making {}: {error}", to.display())))?;
    let entries = std::fs::read_dir(from)
        .map_err(|error| Error::new(format!("reading {}: {error}", from.display())))?;
    for entry in entries {
        let entry = entry.map_err(|error| Error::new(format!("reading a directory: {error}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if skipped(&name) {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        let meta = std::fs::symlink_metadata(&source)
            .map_err(|error| Error::new(format!("reading {}: {error}", source.display())))?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            copy_tree(&source, &target)?;
        } else {
            let _size = std::fs::copy(&source, &target)
                .map_err(|error| Error::new(format!("copying {}: {error}", source.display())))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_up_file_names_where_the_tree_is() {
        let text = String::from_utf8(ZSHRC.to_vec()).unwrap();
        assert!(text.contains(&format!("export ZSH=/{DIRECTORY}")));
        assert!(text.contains("source $ZSH/oh-my-zsh.sh"));
        // Nothing it writes goes anywhere but /tmp.
        assert!(text.contains("export ZSH_CACHE_DIR=/tmp/oh-my-zsh"));
        assert!(text.contains("export ZSH_COMPDUMP=/tmp/oh-my-zsh/zcompdump"));
        // And it is not audited: the tree is root's, and 0755.
        assert!(text.contains("ZSH_DISABLE_COMPFIX=true"));
    }

    #[test]
    fn a_tree_is_read_in_name_order_without_the_projects_own_machinery() {
        let dir = std::env::temp_dir().join(format!("xtask-omz-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), b"ref: refs/heads/master").unwrap();
        std::fs::write(dir.join("oh-my-zsh.sh"), b"# sourced\n").unwrap();
        std::fs::write(dir.join("lib").join("git.zsh"), b"# sourced\n").unwrap();
        std::fs::write(dir.join("tool.sh"), b"#!/bin/sh\n").unwrap();
        let mut files = Vec::new();
        read_tree(&dir, DIRECTORY, &mut files).unwrap();
        let names: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(
            names,
            [
                DIRECTORY,
                "usr/share/oh-my-zsh/lib",
                "usr/share/oh-my-zsh/lib/git.zsh",
                "usr/share/oh-my-zsh/oh-my-zsh.sh",
                "usr/share/oh-my-zsh/tool.sh",
            ]
        );
        // A script is executable, anything else is not.
        let mode = |name: &str| files.iter().find(|file| file.path == name).unwrap().mode;
        assert_eq!(mode("usr/share/oh-my-zsh/tool.sh"), 0o755);
        assert_eq!(mode("usr/share/oh-my-zsh/oh-my-zsh.sh"), 0o644);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
