//! The nodes `.device` units wait for (`docs/INIT.md` §4.2).
//!
//! The manager asks for a node with `WatchDevice` and is told with
//! `Event::Device` whether it is there: at once, then at each change. The
//! watch is inotify on the node's directory, or, while that directory does
//! not exist yet (`/dev/dri` before the first card), on the nearest one
//! above it that does, moved down the path as directories appear. Whatever
//! an inotify event says, every watched node is looked at again with a
//! `stat`, so a missed or merged event costs nothing but the look.
//!
//! The answer to a watch is a `stat` made after the watch is armed, so a
//! node that appears between the two is seen by one or the other. Should
//! the kernel have no inotify, or refuse a watch, every watched node is
//! looked at once a second instead, and init says so once.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use ferrix_svc::event::UnitId;

use crate::{say, sys};

/// What a directory is watched for: an entry made, removed or moved in or
/// out, and the directory itself going.
const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;

/// How often nodes are looked at without inotify.
const POLL: Duration = Duration::from_secs(1);

/// The most passes [`Devices::arm`] makes: one per directory a path can
/// gain while it arms, which no path under `/dev` comes near.
const PASSES: usize = 16;

/// `struct inotify_event` without its name.
const HEADER: usize = 16;

/// One watched node.
#[derive(Debug)]
struct Node {
    path: String,
    /// What the manager was last told; `None` before the first answer.
    present: Option<bool>,
}

/// Every device unit's watch.
#[derive(Debug)]
pub(crate) struct Devices {
    /// The inotify instance, unless the kernel has none.
    inotify: Option<File>,
    nodes: BTreeMap<UnitId, Node>,
    /// The directories watched, and their watch descriptors.
    armed: BTreeMap<String, i32>,
    /// Whether nodes are looked at once a second, inotify having failed.
    polling: bool,
    /// When they were last looked at.
    polled: Instant,
}

impl Devices {
    /// Watches over `inotify`, or by looking once a second without one.
    pub(crate) fn new(inotify: Option<OwnedFd>) -> Self {
        Self {
            polling: inotify.is_none(),
            inotify: inotify.map(File::from),
            nodes: BTreeMap::new(),
            armed: BTreeMap::new(),
            polled: Instant::now(),
        }
    }

    /// Watch `path` for `unit`: whether it is there, to tell the manager.
    pub(crate) fn watch(&mut self, unit: UnitId, path: String) -> Vec<(UnitId, bool)> {
        let _ = self.nodes.insert(
            unit,
            Node {
                path,
                present: None,
            },
        );
        self.arm();
        self.look()
    }

    /// Stop watching `unit`'s node.
    pub(crate) fn unwatch(&mut self, unit: UnitId) {
        if self.nodes.remove(&unit).is_some() {
            self.arm();
        }
    }

    /// The inotify descriptor, for epoll.
    pub(crate) fn fd(&self) -> Option<RawFd> {
        self.inotify.as_ref().map(File::as_raw_fd)
    }

    /// When the nodes are next to be looked at without inotify, if they are.
    pub(crate) fn next_poll(&self) -> Option<Instant> {
        (self.polling && !self.nodes.is_empty()).then(|| self.polled + POLL)
    }

    /// The inotify descriptor is readable: read what it says, move the
    /// watches to where they now belong, and look at every node.
    pub(crate) fn changed(&mut self) -> Vec<(UnitId, bool)> {
        let dropped = self.inotify.as_ref().map(drain).unwrap_or_default();
        // The kernel dropped these watches itself, their directory gone.
        self.armed.retain(|_, wd| !dropped.contains(wd));
        self.arm();
        self.look()
    }

    /// Look at every node, when it is time to without inotify.
    pub(crate) fn poll(&mut self) -> Vec<(UnitId, bool)> {
        if self.next_poll().is_none_or(|at| at > Instant::now()) {
            return Vec::new();
        }
        self.polled = Instant::now();
        self.arm();
        self.look()
    }

    /// Watch the directory each node is waited for in, and nothing else.
    /// A directory may appear while this runs, so it goes round until a
    /// pass arms nothing new.
    fn arm(&mut self) {
        let Some(fd) = self.fd() else {
            return;
        };
        for _ in 0..PASSES {
            let wanted: BTreeSet<String> = self
                .nodes
                .values()
                .filter_map(|node| watch_point(&node.path))
                .collect();
            let stale: Vec<(String, i32)> = self
                .armed
                .iter()
                .filter(|(dir, _)| !wanted.contains(*dir))
                .map(|(dir, wd)| (dir.clone(), *wd))
                .collect();
            for (dir, wd) in stale {
                let _ = self.armed.remove(&dir);
                // Two paths to one directory share its descriptor.
                if !self.armed.values().any(|other| *other == wd) {
                    sys::inotify_rm(fd, wd);
                }
            }
            let mut added = false;
            for dir in wanted {
                if self.armed.contains_key(&dir) {
                    continue;
                }
                let Ok(c_dir) = CString::new(dir.as_str()) else {
                    continue;
                };
                match sys::inotify_add(fd, &c_dir, MASK) {
                    Ok(wd) => {
                        let _ = self.armed.insert(dir, wd);
                        added = true;
                    }
                    Err(error) => self.refused(&dir, &error),
                }
            }
            if !added {
                return;
            }
        }
    }

    /// inotify refused to watch `dir`: look at the nodes once a second
    /// from now on, and say so the first time.
    fn refused(&mut self, dir: &str, error: &io::Error) {
        if !self.polling {
            say(&format!(
                "watching {dir} with inotify failed: {error}; \
                 device nodes are looked at once a second"
            ));
        }
        self.polling = true;
    }

    /// `stat` every node, and say which changed since the manager was last
    /// told, and which it has not been told about yet.
    fn look(&mut self) -> Vec<(UnitId, bool)> {
        let mut changed = Vec::new();
        for (&unit, node) in &mut self.nodes {
            let present = fs::metadata(&node.path).is_ok();
            if node.present != Some(present) {
                node.present = Some(present);
                changed.push((unit, present));
            }
        }
        changed
    }
}

/// The directory to watch for `path`: its own, or the nearest above it
/// that exists.
fn watch_point(path: &str) -> Option<String> {
    Path::new(path)
        .parent()?
        .ancestors()
        .find(|dir| dir.is_dir())
        .and_then(Path::to_str)
        .map(str::to_owned)
}

/// Read every event `inotify` holds; the watch descriptors the kernel
/// dropped among them.
fn drain(mut inotify: &File) -> Vec<i32> {
    let mut dropped = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match inotify.read(&mut buffer) {
            Ok(0) => return dropped,
            Ok(got) => dropped.extend(ignored(buffer.get(..got).unwrap_or_default())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return dropped,
            Err(error) => {
                say(&format!("reading inotify failed: {error}"));
                return dropped;
            }
        }
    }
}

/// The watch descriptors a read of inotify events says the kernel dropped
/// (`IN_IGNORED`).
fn ignored(mut bytes: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    while let Some(header) = bytes.get(..HEADER) {
        let word = |at: usize| -> [u8; 4] {
            header
                .get(at..at + 4)
                .and_then(|four| four.try_into().ok())
                .unwrap_or_default()
        };
        let wd = i32::from_ne_bytes(word(0));
        let mask = u32::from_ne_bytes(word(4));
        let len = usize::try_from(u32::from_ne_bytes(word(12))).unwrap_or(usize::MAX);
        if mask & libc::IN_IGNORED != 0 {
            out.push(wd);
        }
        bytes = bytes.get(HEADER.saturating_add(len)..).unwrap_or_default();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node whose directory does not exist yet: the watch starts above
    /// it, moves down as the directory appears, and the node's coming and
    /// going are each said once.
    #[test]
    fn a_node_is_seen_through_a_directory_made_after_the_watch() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!("init-devices-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        let dri = root.join("dri");
        let card = dri.join("card0");
        let card_path = card.to_str().unwrap_or_default().to_owned();
        let mut devices = Devices::new(Some(sys::inotify()?));
        let unit = UnitId(7);
        assert_eq!(devices.watch(unit, card_path), [(unit, false)]);
        assert_eq!(
            devices.armed.keys().collect::<Vec<_>>(),
            [&root.display().to_string()]
        );
        fs::create_dir(&dri)?;
        assert!(
            devices.changed().is_empty(),
            "the directory alone is no node"
        );
        assert_eq!(
            devices.armed.keys().collect::<Vec<_>>(),
            [&dri.display().to_string()]
        );
        drop(File::create(&card)?);
        assert_eq!(devices.changed(), [(unit, true)]);
        fs::remove_file(&card)?;
        assert_eq!(devices.changed(), [(unit, false)]);
        devices.unwatch(unit);
        assert!(devices.armed.is_empty());
        fs::remove_dir_all(&root)
    }

    /// A node there already is said to be at once.
    #[test]
    fn a_node_already_there_is_present_at_once() -> io::Result<()> {
        let root =
            std::env::temp_dir().join(format!("init-devices-at-once-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let node = root.join("node");
        drop(File::create(&node)?);
        let mut devices = Devices::new(Some(sys::inotify()?));
        let found = devices.watch(UnitId(1), node.display().to_string());
        assert_eq!(found, [(UnitId(1), true)]);
        assert!(devices.changed().is_empty());
        fs::remove_dir_all(&root)
    }
}
