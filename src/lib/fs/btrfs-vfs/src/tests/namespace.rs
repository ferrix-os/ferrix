//! The mount through the VFS itself: a btrfs volume at `/mnt` on a tmpfs root.
//!
//! The tests beside this one call the [`Inode`] trait directly. These go
//! through [`Namespace`], which is what a system call reaches, and check what
//! only the pairing of the two can get wrong: the mount point resolves to
//! btrfs's root while `..` climbs back into tmpfs, `.` and `..` appear in a
//! listing without btrfs ever being asked for them, a change is refused with
//! the error Linux gives inside a read-only mount and across a mount, and an
//! open file outlives the unmount that detached it.

extern crate std;

use std::sync::atomic::{AtomicI64, Ordering};

use ferrix_sync::SpinParker;
use ferrix_vfs::dirent::DirentWriter;
use ferrix_vfs::tmpfs::{HeapStorage, Tmpfs};
use ferrix_vfs::{Clock, Context, Namespace, OpenFlags, RenameMode};

use super::*;

/// The device number the tmpfs root reports; btrfs is mounted as 42.
const ROOT_DEV: u64 = 1;

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

fn reading() -> OpenFlags {
    OpenFlags {
        read: true,
        ..OpenFlags::default()
    }
}

/// A tmpfs root with an empty `/mnt`, not yet mounted over.
fn unmounted() -> (Namespace, Context) {
    let root = Tmpfs::for_kernel(
        ROOT_DEV,
        Arc::new(Ticking::default()),
        Arc::new(HeapStorage::new(1 << 20)),
        0o755,
    );
    let ns = Namespace::new(root, Arc::new(SpinParker));
    let ctx = ns.context();
    ns.mkdir(&ctx, None, b"/mnt", 0o755).unwrap();
    (ns, ctx)
}

/// Mount image `name` at `/mnt`.
fn mount_at_mnt(ns: &Namespace, ctx: &Context, name: &str) {
    let at = ns.resolve(ctx, None, b"/mnt", true).unwrap();
    let (_, packed) = IMAGES.iter().find(|(n, _)| *n == name).unwrap();
    let _mount = ns
        .mount(Btrfs::mount(Image::new(packed), 42, heap()).unwrap(), &at)
        .unwrap();
}

fn mounted(name: &str) -> (Namespace, Context) {
    let (ns, ctx) = unmounted();
    mount_at_mnt(&ns, &ctx, name);
    (ns, ctx)
}

fn under_mnt(path: &[u8]) -> Vec<u8> {
    [b"/mnt/".as_slice(), path].concat()
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads every file of a real image; plain cargo test covers it, Miri runs the small image tests"
)]
fn every_path_reads_back_through_open_and_read() {
    let (ns, ctx) = mounted("zstd");
    for (path, expected) in &manifest() {
        let full = under_mnt(path);
        let shown = String::from_utf8_lossy(&full);
        match expected.kind {
            "file" => {
                let file = ns.open(&ctx, None, &full, &reading(), 0).unwrap();
                let mut contents = Vec::new();
                let mut piece = vec![0u8; 3001];
                loop {
                    let n = file.read(&mut piece).unwrap();
                    if n == 0 {
                        break;
                    }
                    contents.extend_from_slice(&piece[..n]);
                }
                assert_eq!(contents.len() as u64, expected.size, "{shown} size");
                assert_eq!(crc32c(&contents), expected.crc, "{shown} contents");
            }
            "link" => {
                let target = ns.read_link(&ctx, None, &full).unwrap();
                assert_eq!(crc32c(&target), expected.crc, "{shown} target");
            }
            "dir" => {
                let at = ns.resolve(&ctx, None, &full, false).unwrap();
                let stat = ns.stat(&at).unwrap();
                assert_eq!(stat.metadata.kind, FileType::Directory, "{shown}");
                assert_eq!(stat.dev, 42, "{shown} is on the btrfs mount");
            }
            other => panic!("unknown manifest type {other}"),
        }
    }
}

#[test]
fn the_mount_point_is_btrfs_root_and_dot_dot_climbs_out() {
    let (ns, ctx) = unmounted();
    let covered = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    let covered_ino = ns.stat(&covered).unwrap().metadata.ino;
    mount_at_mnt(&ns, &ctx, "none");

    let mnt = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    let stat = ns.stat(&mnt).unwrap();
    assert_eq!(stat.dev, 42, "/mnt is now the btrfs mount");
    assert_eq!(stat.metadata.ino, 256, "and names its top-level directory");

    let up = ns.resolve(&ctx, None, b"/mnt/..", true).unwrap();
    assert_eq!(
        ns.stat(&up).unwrap().dev,
        ROOT_DEV,
        "`..` from btrfs's root is tmpfs"
    );
    assert!(up.same(&ctx.root), "and it is the namespace's root");

    let deep = ns.resolve(&ctx, None, b"/mnt/dir00/sub0", true).unwrap();
    assert_eq!(
        ns.path_of(&deep, &ctx.root),
        b"/mnt/dir00/sub0".to_vec(),
        "a path inside the mount names its way back out"
    );

    let root = ns
        .open(
            &ctx,
            None,
            b"/",
            &OpenFlags {
                directory: true,
                ..reading()
            },
            0,
        )
        .unwrap();
    let mut listed = None;
    root.read_dir(&mut |entry| {
        if entry.name == b"mnt" {
            listed = Some(entry.ino);
        }
        true
    })
    .unwrap();
    assert_eq!(
        listed,
        Some(covered_ino),
        "a listing shows the covered directory's inode, as Linux's does"
    );
}

#[test]
fn a_small_getdents_buffer_lists_dot_entries_and_every_child() {
    let (ns, ctx) = mounted("lzo");
    let dir = ns
        .open(
            &ctx,
            None,
            b"/mnt",
            &OpenFlags {
                directory: true,
                ..reading()
            },
            0,
        )
        .unwrap();
    let mut names = Vec::new();
    // Room for two or three records, so the listing takes many calls and each
    // one resumes from the cursor the last accepted entry carried.
    let mut buf = vec![0u8; 96];
    loop {
        let mut writer = DirentWriter::new(&mut buf);
        dir.read_dir(&mut |entry| {
            let accepted = writer.push(entry.ino, entry.next, entry.kind.dirent_type(), entry.name);
            if accepted {
                names.push(entry.name.to_vec());
            }
            accepted
        })
        .unwrap();
        if writer.used() == 0 {
            break;
        }
    }
    let unique: BTreeSet<Vec<u8>> = names.iter().cloned().collect();
    assert_eq!(unique.len(), names.len(), "no entry is listed twice");
    let mut expected = children(&manifest(), b"");
    let _ = expected.insert(b".".to_vec());
    let _ = expected.insert(b"..".to_vec());
    assert_eq!(
        unique, expected,
        "`.`, `..` and exactly the image's top level"
    );
}

#[test]
fn changes_inside_and_across_the_mount_are_refused() {
    let (ns, ctx) = mounted("none");
    assert_eq!(
        ns.mkdir(&ctx, None, b"/mnt/new", 0o755),
        Err(Errno::EROFS),
        "mkdir"
    );
    assert_eq!(
        ns.unlink(&ctx, None, b"/mnt/big.txt"),
        Err(Errno::EROFS),
        "unlink"
    );
    assert_eq!(
        ns.rename(
            &ctx,
            (None, b"/mnt/big.txt"),
            (None, b"/mnt/moved"),
            RenameMode::Replace
        ),
        Err(Errno::EROFS),
        "rename within the mount"
    );
    assert_eq!(
        ns.rename(
            &ctx,
            (None, b"/mnt/big.txt"),
            (None, b"/big.txt"),
            RenameMode::Replace
        ),
        Err(Errno::EXDEV),
        "rename out of the mount"
    );
    assert_eq!(
        ns.link(&ctx, (None, b"/mnt/big.txt"), false, (None, b"/hard")),
        Err(Errno::EXDEV),
        "link across the mount"
    );
    let writing = OpenFlags {
        write: true,
        ..reading()
    };
    match ns.open(&ctx, None, b"/mnt/big.txt", &writing, 0) {
        Ok(file) => assert_eq!(file.write(b"x"), Err(Errno::EROFS), "write"),
        Err(errno) => assert_eq!(errno, Errno::EROFS, "open for writing"),
    }
}

#[test]
fn an_open_file_outlives_the_unmount() {
    let (ns, ctx) = mounted("zlib");
    let file = ns.open(&ctx, None, b"/mnt/big.txt", &reading(), 0).unwrap();
    let mnt = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    ns.unmount(&mnt).unwrap();

    let after = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    assert_eq!(
        ns.stat(&after).unwrap().dev,
        ROOT_DEV,
        "/mnt is tmpfs again"
    );
    let mut head = [0u8; 26];
    assert_eq!(
        file.read(&mut head),
        Ok(26),
        "the detached file still reads"
    );
    assert_eq!(
        &head, b"compressible line of text\n",
        "and reads its own bytes"
    );
}

/// A file whose pages are cached keeps its inode, and with it those pages,
/// after the dentry cache's queue has let go of its dentry, until the mount
/// goes: the count of names looked up does not decide how much of the cache
/// stays. On the N150 box (2026-10-18) `tar` making a few thousand files
/// dropped 16 GB of cached game files this way, the dentries' inodes and their
/// pages with them.
#[test]
fn a_cached_file_outlives_its_dentry_leaving_the_queue() {
    let root = Tmpfs::for_kernel(
        ROOT_DEV,
        Arc::new(Ticking::default()),
        Arc::new(HeapStorage::new(1 << 20)),
        0o755,
    );
    let ns = Namespace::with_cache(root, 4, Arc::new(SpinParker));
    let ctx = ns.context();
    ns.mkdir(&ctx, None, b"/mnt", 0o755).unwrap();
    mount_at_mnt(&ns, &ctx, "zlib");
    let file = ns.open(&ctx, None, b"/mnt/big.txt", &reading(), 0).unwrap();
    let mut piece = vec![0u8; 4096];
    while file.read(&mut piece).unwrap() != 0 {}
    drop(file);
    let inode = Arc::downgrade(
        &ns.resolve(&ctx, None, b"/mnt/big.txt", true)
            .unwrap()
            .inode()
            .unwrap(),
    );
    // Misses enough to push every dentry looked up so far out of the queue.
    for i in 0..64 {
        let _ = ns.resolve(&ctx, None, std::format!("/mnt/miss-{i}").as_bytes(), true);
    }
    assert!(ns.cached() <= 4);
    assert!(
        inode.upgrade().is_some(),
        "the cached file's inode went with its dentry"
    );
    assert!(ns.kept_for_pages() >= 1);
    // Unmounting lets go of what was kept for the mount's files.
    let mnt = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    ns.unmount(&mnt).unwrap();
    drop(mnt);
    assert_eq!(ns.kept_for_pages(), 0);
    assert!(inode.upgrade().is_none(), "the unmount kept the inode");
}
