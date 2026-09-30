//! Fuzz the VFS with sequences of operations a program could make.
//!
//! From stage 8 every path a program passes to `open`, `mkdir`, `rename` or
//! `symlink` reaches `src/lib/fs/vfs` in ring 0, and the order of those calls is the
//! program's to choose. The path walk follows symbolic links a program wrote,
//! crosses mounts, and caches both hits and misses; tmpfs takes up to four
//! inode locks for one `rename`. Neither kind of bug shows as a crash on the
//! first call — they show as a name that exists and cannot be found, or a
//! directory listing that disagrees with a lookup.
//!
//! # The properties
//!
//! * Nothing panics, whatever the sequence. With `overflow-checks` on, that
//!   includes every length and offset calculation.
//! * **A listing agrees with a lookup.** Every name `getdents64` reports in a
//!   directory resolves, without following links, to the inode number the
//!   listing gave. This is the property the dentry cache can break: a stale
//!   negative entry hides a file the filesystem has.
//! * **Records round-trip.** What the `getdents64` packer writes, a reader
//!   that knows only the layout reads back as the same entries.
//! * **A write reads back.** Bytes written at an offset read back at it.
//! * **`..` in a listing is where `..` walks to.** The listing's inode number
//!   for `..` is the one `dir/..` resolves to, across mounts too. It carried
//!   the directory's own number until a review found it.
//! * **The tree is a tree.** After every operation, walking down from the
//!   root by listings reaches no directory twice, every name reports
//!   `path_of` as the path it was listed under, and every dentry's parents
//!   end. A rename that moves a directory into its own subtree breaks the
//!   first, and a dentry left with a stale parent breaks the second. With
//!   binds the same directory is reached through more than one mount, so a
//!   directory counts as reached twice only through the same mount.
//! * **Every mount's parents end** (`docs/NAMESPACES.md` §10): binds,
//!   recursive binds, remounts and `MNT_DETACH` of a subtree never leave a
//!   mount that is its own ancestor.
//!
//! Paths are built from a tiny alphabet — three names, `.`, `..`, and a link
//! — so that operations collide with each other constantly rather than
//! scattering over names nothing else touches.

#![no_main]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use ferrix_vfs::dirent::{DirentWriter, records};
use ferrix_vfs::tmpfs::{HeapStorage, Tmpfs};
use ferrix_vfs::{
    Clock, Context, FileSystem, FileType, MountFlags, Namespace, OpenFlags, RenameMode,
    Timespec, Whence,
};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Default)]
struct Ticking(AtomicI64);

impl Clock for Ticking {
    fn now(&self) -> Timespec {
        Timespec {
            tv_sec: self.0.fetch_add(1, Ordering::Relaxed),
            tv_nsec: 0,
        }
    }
}

fn tmpfs(device: u64) -> Arc<dyn FileSystem> {
    // Charged to nobody, as the library's own tests make theirs: `new`
    // charges the running job and can refuse, which a fuzzer has no job for.
    Tmpfs::for_kernel(
        device,
        Arc::new(Ticking::default()),
        Arc::new(HeapStorage::new(1 << 20)),
        0o755,
    )
}

/// Hands out the fuzzer's bytes as the shapes this target needs.
struct Input<'a> {
    rest: &'a [u8],
}

impl Input<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (first, tail) = self.rest.split_first()?;
        self.rest = tail;
        Some(*first)
    }

    /// A path of up to four components from the small alphabet.
    fn path(&mut self) -> Option<Vec<u8>> {
        const PARTS: [&[u8]; 8] = [b"a", b"b", b"c", b".", b"..", b"l", b"", b"mnt"];
        let shape = self.byte()?;
        let mut path = Vec::new();
        if shape & 0x80 != 0 {
            path.push(b'/');
        }
        for _ in 0..=(shape & 0x03) {
            let part = PARTS[usize::from(self.byte()? % 8)];
            path.extend_from_slice(part);
            path.push(b'/');
        }
        if shape & 0x40 == 0 {
            let _ = path.pop();
        }
        if path.is_empty() {
            path.push(b'.');
        }
        Some(path)
    }
}

/// Every name a listing of `dir` reports resolves to the inode it reported.
fn listing_agrees(ns: &Namespace, ctx: &Context, dir: &[u8]) {
    let flags = OpenFlags {
        read: true,
        directory: true,
        ..OpenFlags::default()
    };
    let Ok(file) = ns.open(ctx, None, dir, &flags, 0) else {
        return;
    };
    let mut emitted = Vec::new();
    let mut buf = [0_u8; 256];
    loop {
        let mut writer = DirentWriter::new(&mut buf);
        let mut batch = Vec::new();
        file.read_dir(&mut |entry| {
            let fits = writer.push(entry.ino, entry.next, entry.kind.dirent_type(), entry.name);
            if fits {
                batch.push((entry.ino, entry.name.to_vec()));
            }
            fits
        })
        .expect("a directory that opened lists");
        let used = writer.used();
        if used == 0 {
            break;
        }
        let read: Vec<(u64, Vec<u8>)> = records(&buf[..used])
            .map(|record| (record.ino, record.name.to_vec()))
            .collect();
        assert_eq!(read, batch, "getdents64 records did not read back");
        emitted.extend(batch);
    }
    for (ino, name) in emitted {
        if name == b".." {
            let mut up = dir.to_vec();
            up.extend_from_slice(b"/..");
            let parent = ns.resolve(ctx, None, &up, true).expect("a listed directory has ..");
            let stat = ns.stat(&parent).expect("a directory's parent stats");
            assert_eq!(stat.metadata.ino, ino, "a listing's .. is not where .. walks to");
            continue;
        }
        if name == b"." {
            continue;
        }
        let mut path = dir.to_vec();
        path.push(b'/');
        path.extend_from_slice(&name);
        let at = ns
            .resolve(ctx, None, &path, false)
            .expect("a listed name resolves");
        // A mount point is listed with the inode it covers and resolves to
        // the mounted root, on Linux as here: `readdir` reads the directory's
        // own filesystem, and a lookup crosses the mount. The first thing
        // this target found was that it had asserted otherwise.
        if Arc::ptr_eq(&at.dentry, at.mount.root()) {
            continue;
        }
        let stat = ns.stat(&at).expect("a resolved name stats");
        assert_eq!(stat.metadata.ino, ino, "a listed name resolved elsewhere");
    }
}

/// Every mount's parent chain ends: see the module documentation.
fn parents_end(ns: &Namespace) {
    for mount in ns.mounts() {
        let mut parent = mount.parent();
        for _ in 0..=1024 {
            let Some((above, _)) = parent else { break };
            parent = above.parent();
        }
        assert!(parent.is_none(), "a mount is its own ancestor");
    }
}

/// The tree is a tree: see the module documentation.
///
/// Walks down from the root by listing each directory and resolving each name
/// in it without following links, so a symbolic link cannot make a loop of
/// its own. A directory is known by its device and inode number, which a
/// directory has one of: no hard links to one exist.
fn tree_is_a_tree(ns: &Namespace, ctx: &Context) {
    let flags = OpenFlags {
        read: true,
        directory: true,
        ..OpenFlags::default()
    };
    let mut seen = HashSet::new();
    let mut pending = vec![b"/".to_vec()];
    while let Some(path) = pending.pop() {
        let at = ns.resolve(ctx, None, &path, false).expect("a listed name resolves");
        assert_eq!(
            ns.path_of(&at, &ctx.root),
            path,
            "a name does not know where it is"
        );
        let mut dentry = Some(Arc::clone(&at.dentry));
        for _ in 0..=64 {
            let Some(here) = dentry else { break };
            dentry = here.parent();
        }
        assert!(dentry.is_none(), "a dentry is its own ancestor");

        let stat = ns.stat(&at).expect("a resolved name stats");
        if stat.metadata.kind != FileType::Directory {
            continue;
        }
        assert!(
            seen.insert((at.mount.id(), stat.dev, stat.metadata.ino)),
            "a directory is inside itself: reached again at {}",
            String::from_utf8_lossy(&path)
        );
        let file = ns.open(ctx, None, &path, &flags, 0).expect("a directory opens");
        file.read_dir(&mut |entry| {
            if entry.name != b"." && entry.name != b".." {
                let mut child = path.clone();
                if child != b"/" {
                    child.push(b'/');
                }
                child.extend_from_slice(entry.name);
                pending.push(child);
            }
            true
        })
        .expect("a directory lists");
    }
}

fuzz_target!(|data: &[u8]| {
    let ns = Namespace::with_cache(tmpfs(1), 16, Arc::new(ferrix_sync::SpinParker));
    let ctx = ns.context();
    let _ = ns.mkdir(&ctx, None, b"/mnt", 0o755);
    let mut input = Input { rest: data };
    let mut devices = 2_u64;

    for _ in 0..64 {
        let Some(op) = input.byte() else {
            break;
        };
        let Some(path) = input.path() else {
            break;
        };
        match op % 15 {
            0 => {
                let _ = ns.mkdir(&ctx, None, &path, 0o755);
            }
            1 => {
                let flags = OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    truncate: op & 0x10 != 0,
                    append: op & 0x20 != 0,
                    ..OpenFlags::default()
                };
                if let Ok(file) = ns.open(&ctx, None, &path, &flags, 0o644) {
                    let offset = i64::from(input.byte().unwrap_or(0)) * 97;
                    let len = usize::from(input.byte().unwrap_or(0));
                    let payload: Vec<u8> = (0..len).map(|i| i as u8 ^ op).collect();
                    let append = flags.append;
                    if file.seek(offset, Whence::Set).is_ok()
                        && let Ok(written) = file.write(&payload)
                        && !append
                    {
                        let mut back = vec![0_u8; written];
                        let got = file.read_at(offset as u64, &mut back).expect("reads back");
                        assert_eq!(&back[..got], &payload[..got], "a write did not read back");
                        assert_eq!(got, written, "a write read back short");
                    }
                }
            }
            2 => {
                let _ = ns.unlink(&ctx, None, &path);
            }
            3 => {
                let _ = ns.rmdir(&ctx, None, &path);
            }
            4 | 5 => {
                let Some(other) = input.path() else { break };
                let mode = if op & 0x10 == 0 {
                    RenameMode::Replace
                } else {
                    RenameMode::NoReplace
                };
                let _ = ns.rename(&ctx, (None, &path), (None, &other), mode);
            }
            6 => {
                let Some(target) = input.path() else { break };
                let _ = ns.symlink(&ctx, None, &path, &target);
            }
            7 => {
                let Some(other) = input.path() else { break };
                let _ = ns.link(&ctx, (None, &path), op & 0x10 != 0, (None, &other));
            }
            8 => {
                if let Ok(at) = ns.resolve(&ctx, None, &path, true) {
                    let _ = ns.truncate(&at, u64::from(input.byte().unwrap_or(0)) * 131);
                }
            }
            9 => {
                if let Ok(at) = ns.resolve(&ctx, None, &path, true) {
                    devices += 1;
                    let _ = ns.mount(tmpfs(devices), &at);
                }
            }
            10 => {
                if let Ok(at) = ns.resolve(&ctx, None, &path, true) {
                    let _ = ns.unmount(&at);
                }
            }
            12 => {
                let Some(other) = input.path() else { break };
                if let Ok(from) = ns.resolve(&ctx, None, &path, true)
                    && let Ok(onto) = ns.resolve(&ctx, None, &other, true)
                {
                    let _ = ns.bind(&from, &onto, op & 0x10 != 0);
                }
            }
            13 => {
                if let Ok(at) = ns.resolve(&ctx, None, &path, true) {
                    let _ = ns.unmount_with(&at, true);
                }
            }
            14 => {
                if let Ok(at) = ns.resolve(&ctx, None, &path, true) {
                    let flags = if op & 0x10 == 0 {
                        MountFlags::NONE
                    } else {
                        MountFlags::READ_ONLY
                    };
                    let _ = if op & 0x20 == 0 {
                        ns.remount(&at, flags)
                    } else {
                        ns.remount_filesystem(&at, flags)
                    };
                }
            }
            _ => listing_agrees(&ns, &ctx, &path),
        }
        tree_is_a_tree(&ns, &ctx);
        parents_end(&ns);
    }
    listing_agrees(&ns, &ctx, b"/");
    listing_agrees(&ns, &ctx, b"/mnt");
});
