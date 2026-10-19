//! Paths as bytes, and the limits Linux puts on them.
//!
//! A Linux path is a sequence of bytes, not a string: any byte but `/` and NUL
//! may appear in a name, and a filesystem that decoded names as UTF-8 would
//! refuse files a program had every right to create. Nothing here decodes.

use ferrix_linux_abi::errno::Errno;

use crate::Result;

/// The longest name one component may have, in bytes.
pub const NAME_MAX: usize = 255;

/// The longest path a system call accepts, in bytes, including its NUL.
///
/// The system call layer bounds its copy with this, so a path of exactly this
/// many bytes with no terminator is `ENAMETOOLONG` there rather than here.
pub const PATH_MAX: usize = 4096;

/// How many symbolic links one resolution may follow before it is `ELOOP`.
///
/// Linux's `MAXSYMLINKS`. A count rather than cycle detection, because a
/// chain of forty distinct links is as unusable as a cycle and much cheaper
/// to recognise.
pub const MAX_SYMLINKS: u32 = 40;

/// What one component of a path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Component<'a> {
    /// An ordinary name.
    Name(&'a [u8]),
    /// `.`.
    Dot,
    /// `..`.
    DotDot,
}

/// Check that `name` is usable as one component.
///
/// # Errors
///
/// `ENOENT` for an empty name, `ENAMETOOLONG` for one over [`NAME_MAX`], and
/// `EINVAL` for one containing `/` or NUL — which a path walk never produces,
/// but a filesystem's own caller might.
pub fn check_name(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        return Err(Errno::ENOENT);
    }
    if name.len() > NAME_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    if name.iter().any(|&b| b == b'/' || b == 0) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// Classify a component as `.`, `..` or a name.
#[must_use]
pub fn classify(component: &[u8]) -> Component<'_> {
    match component {
        b"." => Component::Dot,
        b".." => Component::DotDot,
        name => Component::Name(name),
    }
}

/// The components of `path`, skipping empty ones.
///
/// `a//b/` yields `a` and `b`. Whether the path was absolute, or ended in a
/// slash, is asked separately with [`is_absolute`] and [`ends_with_slash`],
/// because both change what a resolution means rather than what it walks.
pub fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

/// Whether resolution starts at the root rather than the working directory.
#[must_use]
pub fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

/// Whether the path ends in a slash, which requires what it names to be a
/// directory.
#[must_use]
pub fn ends_with_slash(path: &[u8]) -> bool {
    path.last() == Some(&b'/')
}

/// Split `path` into the directory part and its last component, as `dirname`
/// and `basename` would, without allocating.
///
/// Trailing slashes are ignored for the purpose of finding the last
/// component: `a/b/` splits as `a` and `b`. The directory part of a relative
/// single-component path is empty, and of `/x` is `/`.
#[must_use]
pub fn split_last(path: &[u8]) -> (&[u8], &[u8]) {
    let mut end = path.len();
    while end > 1 && path.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    let trimmed = path.get(..end).unwrap_or(path);
    match trimmed.iter().rposition(|&b| b == b'/') {
        None => (b"", trimmed),
        Some(0) => (
            path.get(..1).unwrap_or(b""),
            trimmed.get(1..).unwrap_or(b""),
        ),
        Some(at) => (
            trimmed.get(..at).unwrap_or(b""),
            trimmed.get(at + 1..).unwrap_or(b""),
        ),
    }
}
