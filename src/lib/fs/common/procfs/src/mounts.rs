//! `/proc/mounts` and `/proc/<pid>/mountinfo`.
//!
//! ```text
//! proc /proc proc rw 0 0
//! ```
//!
//! Six fields separated by single spaces, which is exactly why the first three
//! are escaped: `show_vfsmnt` passes the source, the mount point and the type
//! through `mangle`, which writes a space, tab, newline or backslash as a
//! backslash and three octal digits. `getmntent` undoes it. A mount point with
//! a space in its name printed raw would be read as two fields and every field
//! after it shifted by one.

use alloc::vec::Vec;

use crate::text::put;

/// One mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mount<'a> {
    /// What was mounted: a device path, or the filesystem's name.
    pub source: &'a [u8],
    /// Where, as a path from the reader's root.
    pub point: &'a [u8],
    /// The filesystem type.
    pub fstype: &'a [u8],
    /// The comma-separated options, printed as given.
    pub options: &'a [u8],
}

/// Append one line, newline included.
///
/// The last two fields are the `dump` and `fsck` pass numbers of `fstab`,
/// which Linux always prints as zero.
pub fn render(out: &mut Vec<u8>, mount: &Mount<'_>) {
    mangle(out, mount.source);
    out.push(b' ');
    mangle(out, mount.point);
    out.push(b' ');
    mangle(out, mount.fstype);
    out.push(b' ');
    out.extend_from_slice(mount.options);
    out.extend_from_slice(b" 0 0\n");
}

/// One line of `/proc/<pid>/mountinfo`.
///
/// ```text
/// 28 34 0:25 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
/// ```
///
/// `show_mountinfo`'s fields: the mount's id and its parent's, the
/// filesystem's device, the mount's root inside its filesystem, the mount
/// point from the reader's root, the mount's own options, the optional
/// tagged fields (none: no mount here is ever shared), `-`, the type, the
/// source and the filesystem's own options. The root, the point, the type and
/// the source go through `mangle`, as Linux's `seq_dentry`, `seq_path_root`,
/// `show_type` and `mangle` escape them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountInfo<'a> {
    /// The mount's id: what `statx`'s `stx_mnt_id` reports.
    pub id: u64,
    /// The id of the mount it is on; its own for the namespace's root.
    pub parent: u64,
    /// The filesystem's `st_dev`, as `makedev` encodes it.
    pub device: u64,
    /// The path of the mount's root inside its filesystem.
    pub root: &'a [u8],
    /// Where, as a path from the reader's root.
    pub point: &'a [u8],
    /// The mount's own options: `rw` or `ro`, then `nosuid` and the rest.
    pub options: &'a [u8],
    /// The filesystem type.
    pub fstype: &'a [u8],
    /// What was mounted.
    pub source: &'a [u8],
    /// The filesystem's own options: `rw` or `ro`, and any it keeps.
    pub super_options: &'a [u8],
}

/// Append one `mountinfo` line, newline included.
pub fn render_info(out: &mut Vec<u8>, mount: &MountInfo<'_>) {
    let dev = mount.device;
    // glibc's `gnu_dev_major` and `gnu_dev_minor`: each a 32-bit number.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    put(
        out,
        format_args!("{} {} {major}:{minor} ", mount.id, mount.parent),
    );
    mangle(out, mount.root);
    out.push(b' ');
    mangle(out, mount.point);
    out.push(b' ');
    out.extend_from_slice(mount.options);
    out.extend_from_slice(b" - ");
    mangle(out, mount.fstype);
    out.push(b' ');
    mangle(out, mount.source);
    out.push(b' ');
    out.extend_from_slice(mount.super_options);
    out.push(b'\n');
}

/// `mangle(m, s)`, escaping `" \t\n\\"`.
fn mangle(out: &mut Vec<u8>, field: &[u8]) {
    for &byte in field {
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\\') {
            put(out, format_args!("\\{byte:03o}"));
        } else {
            out.push(byte);
        }
    }
}
