//! Path resolution: from bytes a program passed to a place in the tree.
//!
//! One function, [`Namespace::walk`], which every operation that takes a path
//! goes through, so that `open`, `mkdir`, `rename` and `stat` cannot disagree
//! about what a path means. What they differ in is only what they do with the
//! answer: whether the last component may be missing (a create), whether a
//! symbolic link in last position is followed (`stat` against `lstat`), and
//! what `.` or `..` in last position is an error for.
//!
//! # Symbolic links without recursion
//!
//! A link's target is pushed onto a stack of pending path fragments and the
//! walk carries on from the top of it. Recursion would put a program's choice
//! of link depth onto a kernel stack sixteen kilobytes deep; the stack here is
//! on the heap and bounded by [`MAX_SYMLINKS`] fragments.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;

use crate::Result;
use crate::access::MAY_EXEC;
use crate::dentry::Dentry;
use crate::namespace::{Context, Location, Namespace};
use crate::node::FileType;
use crate::path::{Component, MAX_SYMLINKS, check_name, classify, ends_with_slash, is_absolute};

/// What the last component of a walked path was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LastPart {
    /// An ordinary name, looked up in [`Walked::parent`].
    Name(Box<[u8]>),
    /// `.`.
    Dot,
    /// `..`.
    DotDot,
    /// The path had no components.
    Root,
}

/// Where a walk ended.
#[derive(Debug, Clone)]
pub(crate) struct Walked {
    /// The directory the last component was resolved in.
    pub(crate) parent: Location,
    /// What the last component was.
    pub(crate) last: LastPart,
    /// What it resolved to, after crossing any mount on it. Negative only when
    /// `last` is a name that does not exist.
    pub(crate) found: Location,
    /// The path ended in a slash, so what it names must be a directory.
    pub(crate) must_be_dir: bool,
}

impl Walked {
    /// The name, or `err` if the last component was not one.
    pub(crate) fn name_or(&self, err: Errno) -> Result<&[u8]> {
        match &self.last {
            LastPart::Name(name) => Ok(name),
            _ => Err(err),
        }
    }

    /// Whether the name is a mount point: what it resolved to is on another
    /// mount than the directory holding it, or it is covered by a mount the
    /// walk did not cross because it came through another bind of the same
    /// filesystem (Linux's `d_mountpoint`, which `vfs_rmdir`, `vfs_unlink`
    /// and `vfs_rename` ask whatever mount the path went through).
    pub(crate) fn is_mountpoint(&self) -> bool {
        !Arc::ptr_eq(&self.found.mount, &self.parent.mount) || self.found.dentry.is_mountpoint()
    }
}

/// Where a walk in progress has got to.
struct Cursor {
    /// The directory the most recent component was resolved in.
    parent: Location,
    /// What it resolved to.
    current: Location,
    /// What that component was.
    last: LastPart,
    /// The most recent component must be a directory.
    must_be_dir: bool,
    /// Symbolic links followed so far.
    links: u32,
}

/// One fragment of path still to walk.
struct Fragment {
    bytes: Vec<u8>,
    at: usize,
    /// The fragment is a symbolic link's target that stood in last position
    /// with a slash after it, so its own last component must be a directory.
    must_be_dir: bool,
}

impl Fragment {
    fn rest(&self) -> &[u8] {
        self.bytes.get(self.at..).unwrap_or(&[])
    }
}

/// One component, and what the walk needs to know about its position.
struct Step {
    component: Box<[u8]>,
    is_last: bool,
    must_be_dir: bool,
}

/// The stack of fragments.
struct Steps {
    fragments: Vec<Fragment>,
}

impl Steps {
    fn new(path: &[u8]) -> Steps {
        Steps {
            fragments: vec![Fragment {
                bytes: path.to_vec(),
                at: 0,
                must_be_dir: false,
            }],
        }
    }

    fn push(&mut self, target: Vec<u8>, must_be_dir: bool) {
        self.fragments.push(Fragment {
            bytes: target,
            at: 0,
            must_be_dir,
        });
    }

    fn next(&mut self) -> Option<Step> {
        loop {
            let fragment = self.fragments.last_mut()?;
            let skip = fragment.rest().iter().take_while(|&&b| b == b'/').count();
            fragment.at = fragment.at.saturating_add(skip);
            let rest = fragment.rest();
            if rest.is_empty() {
                let _ = self.fragments.pop();
                continue;
            }
            let len = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
            let component = Box::from(rest.get(..len).unwrap_or(&[]));
            let slash_follows = len < rest.len();
            let inherited = fragment.must_be_dir;
            fragment.at = fragment.at.saturating_add(len);

            let is_last = self
                .fragments
                .iter()
                .all(|f| f.rest().iter().all(|&b| b == b'/'));
            return Some(Step {
                component,
                is_last,
                must_be_dir: is_last && (slash_follows || inherited),
            });
        }
    }
}

impl Namespace {
    /// Resolve `path` from `start`, or from the context's root if it is
    /// absolute.
    ///
    /// `follow_last` decides whether a symbolic link in last position is
    /// followed. One is followed regardless when the path ends in a slash,
    /// which is what makes `ls link/` list the target.
    ///
    /// # Errors
    ///
    /// `ENOENT` for an empty path or a missing directory on the way,
    /// `ENOTDIR` for a non-directory on the way, `ELOOP` past
    /// [`MAX_SYMLINKS`], `ENAMETOOLONG` for a component over
    /// [`crate::path::NAME_MAX`], and whatever a filesystem's lookup refuses
    /// with.
    pub(crate) fn walk(
        &self,
        ctx: &Context,
        start: &Location,
        path: &[u8],
        follow_last: bool,
    ) -> Result<Walked> {
        if path.is_empty() {
            return Err(Errno::ENOENT);
        }
        let current = if is_absolute(path) {
            ctx.root.clone()
        } else {
            start.clone()
        };
        let mut at = Cursor {
            parent: current.clone(),
            current,
            last: LastPart::Root,
            must_be_dir: ends_with_slash(path),
            links: 0,
        };
        let mut steps = Steps::new(path);

        while let Some(step) = steps.next() {
            require_directory(&at.current)?;
            require_search(ctx, &at.current)?;
            at.must_be_dir = step.must_be_dir;
            if let Some(target) = self.advance(ctx, &mut at, &step, follow_last)? {
                steps.push(target, step.must_be_dir);
            }
        }

        if at.must_be_dir
            && let Some(inode) = at.current.dentry.inode()
            && inode.metadata().kind != FileType::Directory
        {
            return Err(Errno::ENOTDIR);
        }
        Ok(Walked {
            parent: at.parent,
            last: at.last,
            found: at.current,
            must_be_dir: at.must_be_dir,
        })
    }

    /// Take one step, returning a symbolic link's target when the step was
    /// onto a link the walk must follow.
    fn advance(
        &self,
        ctx: &Context,
        at: &mut Cursor,
        step: &Step,
        follow_last: bool,
    ) -> Result<Option<Vec<u8>>> {
        let name = match classify(&step.component) {
            Component::Dot => {
                at.parent = at.current.clone();
                at.last = LastPart::Dot;
                return Ok(None);
            }
            Component::DotDot => {
                at.parent = at.current.clone();
                at.current = up(&at.current, Some(&ctx.root));
                at.last = LastPart::DotDot;
                return Ok(None);
            }
            Component::Name(name) => name,
        };

        check_name(name)?;
        let child = self.child(&at.current.dentry, name)?;
        let next = self.descend_mounts(Location {
            mount: Arc::clone(&at.current.mount),
            dentry: child,
        });
        let Some(inode) = next.dentry.inode() else {
            if !step.is_last {
                return Err(Errno::ENOENT);
            }
            at.parent = core::mem::replace(&mut at.current, next);
            at.last = LastPart::Name(Box::from(name));
            return Ok(None);
        };

        let follow = !step.is_last || follow_last || step.must_be_dir;
        if !follow || inode.metadata().kind != FileType::Symlink {
            at.parent = core::mem::replace(&mut at.current, next);
            at.last = LastPart::Name(Box::from(name));
            return Ok(None);
        }

        at.links = at.links.saturating_add(1);
        if at.links > MAX_SYMLINKS {
            return Err(Errno::ELOOP);
        }
        // A magic link is not a path: the walk jumps to the object it stands
        // for, and resolves the rest of the path from there. The directory
        // that holds the object is the one the component counts as resolved
        // in, and it has no name to be created or removed by: nothing that
        // needs one may be done through a magic link.
        if let Some(target) = inode.link_location() {
            let target = target?;
            at.parent = up(&target, Some(&ctx.root));
            at.current = target;
            at.last = LastPart::Root;
            return Ok(None);
        }
        let target = inode.read_link()?;
        if target.is_empty() {
            return Err(Errno::ENOENT);
        }
        if is_absolute(&target) {
            at.current = ctx.root.clone();
        }
        // A target of only slashes names the root and has no components; what
        // the walk has is then the answer.
        at.parent = at.current.clone();
        at.last = LastPart::Root;
        Ok(Some(target))
    }

    /// The child dentry called `name`, from the cache or the filesystem.
    fn child(&self, dir: &Arc<Dentry>, name: &[u8]) -> Result<Arc<Dentry>> {
        let inode = dir.inode().ok_or(Errno::ENOENT)?;
        if !inode.caches_lookups() && !inode.caches_lookup_of(name) {
            let found = look_up(inode.as_ref(), name)?;
            return dir.uncached_child(name, found);
        }
        // Until an answer is recorded under a generation nothing changed
        // during: a name has one live dentry or none, which is what lets a
        // rename move the only one. `Dentry::insert_looked_up` says why.
        loop {
            if let Some(child) = dir.cached_child(name) {
                return Ok(child);
            }
            let generation = dir.generation();
            let found = look_up(inode.as_ref(), name)?;
            if let Some((child, cached)) = dir.insert_looked_up(name, found.clone(), generation)? {
                if cached {
                    self.remember(&child);
                }
                return Ok(child);
            }
        }
    }

    /// Step onto whatever is mounted on `at`, repeatedly.
    pub(crate) fn descend_mounts(&self, mut at: Location) -> Location {
        while at.dentry.is_mountpoint() {
            let Some(mount) = self.mounted_on(&at) else {
                break;
            };
            at = Location {
                dentry: Arc::clone(mount.root()),
                mount,
            };
        }
        at
    }
}

/// Ask a directory for `name`: the inode, `None` for a miss, or the error.
fn look_up(
    dir: &dyn crate::node::Inode,
    name: &[u8],
) -> Result<Option<Arc<dyn crate::node::Inode>>> {
    match dir.lookup(name) {
        Ok(found) => Ok(Some(found)),
        Err(Errno::ENOENT) => Ok(None),
        Err(other) => Err(other),
    }
}

/// `ENOENT` or `ENOTDIR` unless `at` is a directory.
fn require_directory(at: &Location) -> Result<()> {
    let inode = at.dentry.inode().ok_or(Errno::ENOENT)?;
    if inode.metadata().kind == FileType::Directory {
        Ok(())
    } else {
        Err(Errno::ENOTDIR)
    }
}

/// `EACCES` unless the context may search the directory `at`: Linux's
/// `may_lookup`, made before every component, `.` and `..` included.
fn require_search(ctx: &Context, at: &Location) -> Result<()> {
    let inode = at.dentry.inode().ok_or(Errno::ENOENT)?;
    ctx.who.require(&inode.metadata(), MAY_EXEC)
}

/// `..` from `at`, which never climbs above `root`, nor above the top of the
/// tree `at` is in when there is no `root` to stop at.
///
/// At the root of a mount, `..` means the directory holding the mount point
/// — possibly several mounts up — which is why this is a loop and why the
/// answer is a location in the mount *above*.
///
/// The root is only compared on the way, never assumed to be `at`: a caller
/// asking for `..` without a context's root passes `None` rather than `at`
/// itself, which would stop at once and answer `.`.
pub(crate) fn up(at: &Location, root: Option<&Location>) -> Location {
    let mut here = at.clone();
    loop {
        if root.is_some_and(|root| here.same(root)) {
            return here;
        }
        if Arc::ptr_eq(&here.dentry, here.mount.root()) {
            match here.mount.parent() {
                Some((mount, dentry)) => {
                    here = Location { mount, dentry };
                    continue;
                }
                None => return here,
            }
        }
        return match here.dentry.parent() {
            Some(parent) => Location {
                mount: here.mount,
                dentry: parent,
            },
            None => here,
        };
    }
}
