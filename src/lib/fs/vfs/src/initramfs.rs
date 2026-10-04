//! Unpacking a cpio archive into a namespace.
//!
//! What Linux's `init/initramfs.c` does, through the same operations a
//! program would use — `mkdir`, `open(O_CREAT|O_EXCL)`, `write`, `symlink`,
//! `link`, `mknod` — so the archive exercises the VFS rather than a private
//! shortcut into tmpfs, and so it would unpack the same way into any
//! filesystem.
//!
//! # Following Linux where an archive is odd
//!
//! * **An entry whose name would escape the root** — absolute, or with a
//!   `..` component — is skipped and counted, not honoured. An initramfs is
//!   built by the build, but it is still a file.
//! * **An existing name** is replaced: the old one is removed first. A
//!   directory that already exists is kept and has its attributes updated.
//! * **Hard links.** newc gives every name of a multiply-linked file the same
//!   inode number and puts the data on one of them, conventionally the last.
//!   The first name creates the file; later names link to it, and any that
//!   carry data write it.
//! * **Directory timestamps** are applied at the end, because creating each
//!   child updates its directory's modification time.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

use ferrix_cpio::{Archive, CpioError, Entry, FileType as CpioType};
use ferrix_linux_abi::errno::Errno;

use crate::file::OpenFlags;
use crate::namespace::{Context, Namespace};
use crate::node::{FileType, NewNode, SetAttributes, Timespec};

/// Encode a device number as a 64-bit `dev_t`, the way `makedev(3)` does.
#[must_use]
pub const fn makedev(major: u32, minor: u32) -> u64 {
    let major = major as u64;
    let minor = minor as u64;
    ((major & 0xfff) << 8) | ((major & !0xfff) << 32) | (minor & 0xff) | ((minor & !0xff) << 12)
}

/// What an unpack made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unpacked {
    /// Directories created.
    pub directories: u32,
    /// Regular files created.
    pub files: u32,
    /// Symbolic links created.
    pub symlinks: u32,
    /// Further names given to a file already created.
    pub hard_links: u32,
    /// Device nodes, pipes and sockets created.
    pub nodes: u32,
    /// Bytes of file content written.
    pub bytes: u64,
    /// Entries refused for a name that would escape the root, or of a kind
    /// there is nothing to make of.
    pub skipped: u32,
    /// Entries left out because they are pid 1's inputs ([`is_init_input`]),
    /// which the kernel's init reads from the archive and nothing unpacks.
    pub init_inputs: u32,
}

/// The directory an image carries pid 1's inputs under: the program, the
/// script for its `sh -c` and the list of commands that `cargo xtask` once
/// compiled into the kernel (`src/kernel/src/init.rs`). Read from the archive
/// by the kernel, never created in a filesystem it is unpacked into.
pub const INIT_INPUTS: &[u8] = b".ferrix";

/// Whether `name`, as an archive gives it, is [`INIT_INPUTS`] or anything
/// beneath it, matched as [`unpack`] matches a name: `./` and `/` prefixes
/// and trailing `/` taken off.
#[must_use]
pub fn is_init_input(name: &str) -> bool {
    let name = normalise(name);
    name == INIT_INPUTS
        || name
            .strip_prefix(INIT_INPUTS)
            .is_some_and(|rest| rest.starts_with(b"/"))
}

/// An entry of the archive beneath [`INIT_INPUTS`], as the kernel's init is
/// given it to judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitEntry<'a> {
    /// Its name relative to the root, normalised as [`unpack`] does.
    pub name: &'a [u8],
    /// Whether it is a regular file.
    pub regular: bool,
    /// Whether it is a directory.
    pub directory: bool,
    /// Its link count, as the archive gives it.
    pub links: u32,
    /// Its bytes, borrowed from the archive.
    pub data: &'a [u8],
}

/// Each entry of `archive` that [`is_init_input`], in the archive's order,
/// up to its first malformed header (which [`unpack`] refuses anyway).
pub fn init_entries(archive: &[u8]) -> impl Iterator<Item = InitEntry<'_>> {
    Archive::new(archive)
        .entries()
        .map_while(Result::ok)
        .filter(|entry| entry.is_safe_path() && is_init_input(entry.name))
        .map(|entry| InitEntry {
            name: normalise(entry.name),
            regular: entry.is_file(),
            directory: entry.is_dir(),
            links: entry.nlink,
            data: entry.data,
        })
}

/// Why an unpack stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpackError {
    /// The archive itself is malformed.
    Archive(CpioError),
    /// An entry could not be created.
    Entry {
        /// Its position in the archive, from zero.
        index: usize,
        /// What the VFS said.
        errno: Errno,
    },
}

impl fmt::Display for UnpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnpackError::Archive(why) => write!(f, "the archive is malformed: {why}"),
            UnpackError::Entry { index, errno } => {
                write!(f, "entry {index} could not be created: errno {}", errno.0)
            }
        }
    }
}

/// State carried across entries.
struct Unpacker<'a> {
    ns: &'a Namespace,
    ctx: &'a Context,
    made: Unpacked,
    /// The first name of each multiply-linked file, by `(major, minor, ino)`.
    links: BTreeMap<(u32, u32, u32), Vec<u8>>,
    /// Directory timestamps to apply once everything is in place.
    directory_times: Vec<(Vec<u8>, Timespec)>,
}

/// Unpack `archive` beneath `ctx.root`.
///
/// # Errors
///
/// [`UnpackError`]: the first malformed header or the first entry the VFS
/// refused. What was created before it stays.
pub fn unpack(ns: &Namespace, ctx: &Context, archive: &[u8]) -> Result<Unpacked, UnpackError> {
    let mut unpacker = Unpacker {
        ns,
        ctx,
        made: Unpacked::default(),
        links: BTreeMap::new(),
        directory_times: Vec::new(),
    };
    for (index, entry) in Archive::new(archive).entries().enumerate() {
        let entry = entry.map_err(UnpackError::Archive)?;
        if !entry.is_safe_path() {
            unpacker.made.skipped = unpacker.made.skipped.saturating_add(1);
            continue;
        }
        if is_init_input(entry.name) {
            unpacker.made.init_inputs = unpacker.made.init_inputs.saturating_add(1);
            continue;
        }
        unpacker
            .entry(&entry)
            .map_err(|errno| UnpackError::Entry { index, errno })?;
    }
    for (path, mtime) in unpacker.directory_times.iter().rev() {
        let change = SetAttributes {
            atime: Some(*mtime),
            mtime: Some(*mtime),
            ..SetAttributes::default()
        };
        let at = resolve_or_root(ns, ctx, path).map_err(|errno| UnpackError::Entry {
            index: usize::MAX,
            errno,
        })?;
        ns.set_attributes(&at, &change)
            .map_err(|errno| UnpackError::Entry {
                index: usize::MAX,
                errno,
            })?;
    }
    Ok(unpacker.made)
}

/// The name relative to the root: `./a/b/` becomes `a/b`, and `.` becomes
/// empty, meaning the root itself.
fn normalise(name: &str) -> &[u8] {
    let mut bytes = name.as_bytes();
    loop {
        if let Some(rest) = bytes.strip_prefix(b"./") {
            bytes = rest;
        } else if let Some(rest) = bytes.strip_prefix(b"/") {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some(rest) = bytes.strip_suffix(b"/") {
        bytes = rest;
    }
    if bytes == b"." { b"" } else { bytes }
}

fn resolve_or_root(ns: &Namespace, ctx: &Context, path: &[u8]) -> crate::Result<crate::Location> {
    if path.is_empty() {
        Ok(ctx.root.clone())
    } else {
        ns.resolve(ctx, Some(&ctx.root), path, false)
    }
}

fn time(entry: &Entry<'_>) -> Timespec {
    Timespec {
        tv_sec: i64::from(entry.mtime),
        tv_nsec: 0,
    }
}

impl Unpacker<'_> {
    fn entry(&mut self, entry: &Entry<'_>) -> crate::Result<()> {
        let path = normalise(entry.name);
        match entry.file_type() {
            CpioType::Directory => self.directory(entry, path),
            CpioType::Regular => self.regular(entry, path),
            CpioType::Symlink => {
                self.remove_existing(path)?;
                self.ns
                    .symlink(self.ctx, Some(&self.ctx.root), path, entry.data)?;
                self.made.symlinks = self.made.symlinks.saturating_add(1);
                self.attributes(entry, path, true)
            }
            CpioType::CharDevice | CpioType::BlockDevice | CpioType::Fifo | CpioType::Socket => {
                self.special(entry, path)
            }
            _ => {
                self.made.skipped = self.made.skipped.saturating_add(1);
                Ok(())
            }
        }
    }

    fn directory(&mut self, entry: &Entry<'_>, path: &[u8]) -> crate::Result<()> {
        if !path.is_empty() {
            match self
                .ns
                .mkdir(self.ctx, Some(&self.ctx.root), path, entry.permissions())
            {
                Ok(()) => self.made.directories = self.made.directories.saturating_add(1),
                Err(Errno::EEXIST) => {
                    let at = resolve_or_root(self.ns, self.ctx, path)?;
                    if at.inode()?.metadata().kind != FileType::Directory {
                        self.remove_existing(path)?;
                        self.ns
                            .mkdir(self.ctx, Some(&self.ctx.root), path, entry.permissions())?;
                        self.made.directories = self.made.directories.saturating_add(1);
                    }
                }
                Err(other) => return Err(other),
            }
        }
        self.attributes(entry, path, false)?;
        self.directory_times.push((path.to_vec(), time(entry)));
        Ok(())
    }

    fn regular(&mut self, entry: &Entry<'_>, path: &[u8]) -> crate::Result<()> {
        let key = (entry.dev_major, entry.dev_minor, entry.ino);
        if entry.nlink > 1
            && let Some(first) = self.links.get(&key).cloned()
        {
            self.remove_existing(path)?;
            let root = Some(&self.ctx.root);
            self.ns
                .link(self.ctx, (root, &first), false, (root, path))?;
            self.made.hard_links = self.made.hard_links.saturating_add(1);
            if !entry.data.is_empty() {
                self.write(&first, entry.data, false)?;
            }
            return self.attributes(entry, path, true);
        }

        self.remove_existing(path)?;
        self.write(path, entry.data, true)?;
        self.made.files = self.made.files.saturating_add(1);
        if entry.nlink > 1 {
            let _ = self.links.insert(key, path.to_vec());
        }
        self.attributes(entry, path, true)
    }

    fn special(&mut self, entry: &Entry<'_>, path: &[u8]) -> crate::Result<()> {
        let node = match entry.file_type() {
            CpioType::CharDevice => NewNode::Device {
                kind: FileType::CharDevice,
                rdev: makedev(entry.rdev_major, entry.rdev_minor),
            },
            CpioType::BlockDevice => NewNode::Device {
                kind: FileType::BlockDevice,
                rdev: makedev(entry.rdev_major, entry.rdev_minor),
            },
            CpioType::Fifo => NewNode::Fifo,
            _ => NewNode::Socket,
        };
        self.remove_existing(path)?;
        self.ns.mknod(
            self.ctx,
            Some(&self.ctx.root),
            path,
            node,
            entry.permissions(),
        )?;
        self.made.nodes = self.made.nodes.saturating_add(1);
        self.attributes(entry, path, true)
    }

    /// Write `data` to `path`, creating it if `create`.
    fn write(&mut self, path: &[u8], data: &[u8], create: bool) -> crate::Result<()> {
        let flags = OpenFlags {
            write: true,
            create,
            exclusive: create,
            truncate: !create,
            nofollow: true,
            ..OpenFlags::default()
        };
        let file = self
            .ns
            .open(self.ctx, Some(&self.ctx.root), path, &flags, 0o600)?;
        let mut rest = data;
        while !rest.is_empty() {
            let written = file.write(rest)?;
            if written == 0 {
                return Err(Errno::EIO);
            }
            rest = rest.get(written..).unwrap_or(&[]);
        }
        self.made.bytes = self.made.bytes.saturating_add(data.len() as u64);
        Ok(())
    }

    /// Remove whatever `path` names, if anything, so an entry can replace it.
    fn remove_existing(&self, path: &[u8]) -> crate::Result<()> {
        let root = Some(&self.ctx.root);
        match self.ns.resolve(self.ctx, root, path, false) {
            Ok(at) if at.inode()?.metadata().kind == FileType::Directory => {
                self.ns.rmdir(self.ctx, root, path)
            }
            Ok(_) => self.ns.unlink(self.ctx, root, path),
            Err(Errno::ENOENT) => Ok(()),
            Err(other) => Err(other),
        }
    }

    fn attributes(&self, entry: &Entry<'_>, path: &[u8], with_time: bool) -> crate::Result<()> {
        let at = resolve_or_root(self.ns, self.ctx, path)?;
        let stamp = with_time.then(|| time(entry));
        let change = SetAttributes {
            permissions: (entry.file_type() != CpioType::Symlink).then(|| entry.permissions()),
            uid: Some(entry.uid),
            gid: Some(entry.gid),
            atime: stamp,
            mtime: stamp,
        };
        self.ns.set_attributes(&at, &change)
    }
}
