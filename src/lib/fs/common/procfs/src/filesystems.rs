//! `/proc/filesystems`.
//!
//! `nodev\tproc\n` for a type that lives in memory, `\text4\n` for one that
//! needs a block device.
//!
//! One line per filesystem type the kernel can mount. A type that needs no
//! block device under it -- every one that lives in memory -- starts `nodev`;
//! one that does starts with nothing. Either way a tab follows, then the name
//! `mount -t` takes. An init script greps this file for `devtmpfs` before it
//! mounts `/dev`, so the name is the one Linux registers, not a description.

use alloc::vec::Vec;

/// One filesystem type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filesystem<'a> {
    /// The name `mount -t` takes.
    pub name: &'a [u8],
    /// Whether it mounts without a block device.
    pub nodev: bool,
}

/// Append one line, newline included.
pub fn render(out: &mut Vec<u8>, filesystem: &Filesystem<'_>) {
    if filesystem.nodev {
        out.extend_from_slice(b"nodev");
    }
    out.push(b'\t');
    out.extend_from_slice(filesystem.name);
    out.push(b'\n');
}
