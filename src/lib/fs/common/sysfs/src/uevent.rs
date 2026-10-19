//! A device's `uevent` file: the environment its hotplug event would carry,
//! one `KEY=value` a line.
//!
//! Linux builds it in `dev_uevent`, in a fixed order: the device number and
//! node name first, the node's mode if its class sets one, its type, the
//! driver bound to it, and then whatever its bus and class add. `udev`,
//! `mdev` and `lspci` read the file for exactly those keys, so the order is
//! kept even though nothing here sends an event.

use alloc::vec::Vec;

use crate::text::put;

/// One `KEY=value` line.
pub fn var(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

/// What a device with a node in `/dev` says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node<'a> {
    /// The device number's two halves.
    pub major: u32,
    /// And the minor half.
    pub minor: u32,
    /// Its path beneath `/dev`: `dri/card0`, `input/event0`, `vda`.
    pub name: &'a [u8],
    /// The node's mode when its class sets one, as `mem` sets `0666` on
    /// `null`: printed as `DEVMODE`, and left out when `None`.
    pub mode: Option<u32>,
    /// `DEVTYPE`, for a class whose devices have types: `disk`, `drm_minor`.
    pub kind: Option<&'a [u8]>,
}

/// The lines `dev_uevent` writes for a device with a node: `MAJOR`,
/// `MINOR`, `DEVNAME`, then `DEVMODE` and `DEVTYPE` if it has them.
pub fn node(out: &mut Vec<u8>, node: &Node<'_>) {
    put(
        out,
        format_args!("MAJOR={}\nMINOR={}\n", node.major, node.minor),
    );
    var(out, "DEVNAME", node.name);
    // C's `%#o`: a leading zero, where Rust's `#` would write `0o`.
    if let Some(mode) = node.mode {
        put(out, format_args!("DEVMODE=0{mode:o}\n"));
    }
    if let Some(kind) = node.kind {
        var(out, "DEVTYPE", kind);
    }
}

/// `DRIVER=`, for a device a driver is bound to.
pub fn driver(out: &mut Vec<u8>, name: &[u8]) {
    var(out, "DRIVER", name);
}

/// What a network interface's `uevent` holds: `INTERFACE` and `IFINDEX`, as
/// `netdev_uevent` adds them.
pub fn interface(out: &mut Vec<u8>, name: &[u8], index: u32) {
    var(out, "INTERFACE", name);
    put(out, format_args!("IFINDEX={index}\n"));
}
