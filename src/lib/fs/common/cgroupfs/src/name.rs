//! Which names a cgroup may have.
//!
//! A cgroup is a directory, so a name is a path component first: not empty,
//! not `.` or `..`, no `/`, at most 255 bytes. cgroup adds one rule of its
//! own, from `cgroup_mkdir`: no newline, because `/proc/<pid>/cgroup` and
//! every file that lists paths is read a line at a time. A name that is also
//! the name of an interface file is not refused here but by the directory,
//! which already has an entry of that name (`EEXIST`).

use crate::Refusal;

/// The longest name, in bytes: `NAME_MAX`.
pub const NAME_MAX: usize = 255;

/// Whether `name` may name a new cgroup.
///
/// # Errors
///
/// [`Refusal::Invalid`] for an empty name, `.`, `..`, or one holding `/` or
/// a newline; [`Refusal::TooLong`] past [`NAME_MAX`].
pub fn check(name: &[u8]) -> Result<(), Refusal> {
    if name.is_empty() || name == b"." || name == b".." {
        return Err(Refusal::Invalid);
    }
    if name.len() > NAME_MAX {
        return Err(Refusal::TooLong);
    }
    if name.iter().any(|&byte| byte == b'/' || byte == b'\n') {
        return Err(Refusal::Invalid);
    }
    Ok(())
}
