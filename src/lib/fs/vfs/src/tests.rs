//! Host tests: the VFS against the rules a shell and a C library assume.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, Ordering};

use ferrix_sync::SpinParker;

extern crate std;

use crate::dirent::{DirentWriter, records};
use crate::fd::FdTable;
use crate::initramfs::{self, makedev};
use crate::path::split_last;
use crate::tmpfs::{HeapStorage, Tmpfs};
use crate::{
    Access, Clock, Context, Errno, FileSystem, FileType, Location, MOUNT_MAX, Namespace, OpenFile,
    OpenFlags, RenameMode, Timespec, Whence,
};

/// A clock that advances a second every time it is read.
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
    Tmpfs::for_kernel(
        device,
        Arc::new(Ticking::default()),
        Arc::new(HeapStorage::new(1 << 30)),
        0o755,
    )
}

fn fresh() -> (Namespace, Context) {
    let ns = Namespace::new(tmpfs(1), Arc::new(SpinParker));
    let ctx = ns.context();
    (ns, ctx)
}

const RW_CREATE: OpenFlags = OpenFlags {
    read: true,
    write: true,
    create: true,
    exclusive: false,
    truncate: false,
    append: false,
    directory: false,
    nofollow: false,
    path: false,
    nonblock: false,
};

const READ: OpenFlags = OpenFlags {
    read: true,
    write: false,
    create: false,
    exclusive: false,
    truncate: false,
    append: false,
    directory: false,
    nofollow: false,
    path: false,
    nonblock: false,
};

fn write_file(ns: &Namespace, ctx: &Context, path: &str, data: &[u8]) {
    let file = ns
        .open(ctx, None, path.as_bytes(), &RW_CREATE, 0o644)
        .unwrap();
    assert_eq!(file.write(data).unwrap(), data.len());
}

fn read_file(ns: &Namespace, ctx: &Context, path: &str) -> Result<Vec<u8>, Errno> {
    let file = ns.open(ctx, None, path.as_bytes(), &READ, 0)?;
    let mut out = Vec::new();
    let mut buf = [0_u8; 7];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn names(file: &OpenFile) -> Vec<String> {
    let mut all = Vec::new();
    file.read_dir(&mut |entry| {
        all.push(String::from_utf8_lossy(entry.name).into_owned());
        true
    })
    .unwrap();
    all
}

fn kind(ns: &Namespace, ctx: &Context, path: &str, follow: bool) -> Result<FileType, Errno> {
    let at = ns.resolve(ctx, None, path.as_bytes(), follow)?;
    Ok(ns.stat(&at)?.metadata.kind)
}

// -- Paths -------------------------------------------------------------------

#[test]
fn split_last_matches_dirname_and_basename() {
    assert_eq!(split_last(b"a/b/c"), (&b"a/b"[..], &b"c"[..]));
    assert_eq!(split_last(b"a/b/"), (&b"a"[..], &b"b"[..]));
    assert_eq!(split_last(b"/x"), (&b"/"[..], &b"x"[..]));
    assert_eq!(split_last(b"x"), (&b""[..], &b"x"[..]));
}

#[test]
fn an_empty_path_is_enoent_and_root_is_a_directory() {
    let (ns, ctx) = fresh();
    assert_eq!(
        ns.resolve(&ctx, None, b"", true).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(kind(&ns, &ctx, "/", true), Ok(FileType::Directory));
    assert_eq!(kind(&ns, &ctx, "///", true), Ok(FileType::Directory));
}

#[test]
fn a_name_longer_than_name_max_is_refused() {
    let (ns, ctx) = fresh();
    let long = vec![b'a'; 256];
    assert_eq!(
        ns.mkdir(&ctx, None, &long, 0o755).unwrap_err(),
        Errno::ENAMETOOLONG
    );
    assert!(ns.mkdir(&ctx, None, &long[..255], 0o755).is_ok());
}

// -- Files -------------------------------------------------------------------

#[test]
fn a_file_reads_back_what_was_written_and_stat_agrees() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/hello", b"hello, world\n");
    assert_eq!(read_file(&ns, &ctx, "/hello").unwrap(), b"hello, world\n");
    let at = ns.resolve(&ctx, None, b"/hello", true).unwrap();
    let stat = ns.stat(&at).unwrap();
    assert_eq!(stat.metadata.size, 13);
    assert_eq!(stat.metadata.mode(), 0o100_644);
    assert_eq!(stat.dev, 1);
}

#[test]
fn a_file_spanning_pages_reads_back_whole() {
    let (ns, ctx) = fresh();
    let data: Vec<u8> = (0..10_000_u32).map(|i| (i % 251) as u8).collect();
    write_file(&ns, &ctx, "/big", &data);
    assert_eq!(read_file(&ns, &ctx, "/big").unwrap(), data);
}

#[test]
fn exclusive_create_refuses_an_existing_name_even_a_dangling_link() {
    let (ns, ctx) = fresh();
    ns.symlink(&ctx, None, b"/dangling", b"/nowhere").unwrap();
    let flags = OpenFlags {
        exclusive: true,
        ..RW_CREATE
    };
    assert_eq!(
        ns.open(&ctx, None, b"/dangling", &flags, 0o644)
            .unwrap_err(),
        Errno::EEXIST
    );
    // Without O_EXCL the link is followed and its target created.
    let _ = ns
        .open(&ctx, None, b"/dangling", &RW_CREATE, 0o644)
        .unwrap();
    assert_eq!(kind(&ns, &ctx, "/nowhere", false), Ok(FileType::Regular));
}

#[test]
fn truncate_on_open_empties_and_append_writes_at_the_end() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", b"0123456789");
    let append = OpenFlags {
        append: true,
        ..RW_CREATE
    };
    let file = ns.open(&ctx, None, b"/f", &append, 0).unwrap();
    let _ = file.seek(0, Whence::Set).unwrap();
    let _ = file.write(b"AB").unwrap();
    assert_eq!(read_file(&ns, &ctx, "/f").unwrap(), b"0123456789AB");

    let trunc = OpenFlags {
        truncate: true,
        ..RW_CREATE
    };
    let _ = ns.open(&ctx, None, b"/f", &trunc, 0).unwrap();
    assert_eq!(read_file(&ns, &ctx, "/f").unwrap(), b"");
}

#[test]
fn shrinking_then_growing_reads_zeros_not_old_bytes() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", &[0xAA; 5000]);
    let at = ns.resolve(&ctx, None, b"/f", true).unwrap();
    ns.truncate(&at, 10).unwrap();
    ns.truncate(&at, 5000).unwrap();
    let back = read_file(&ns, &ctx, "/f").unwrap();
    assert_eq!(&back[..10], &[0xAA; 10]);
    assert!(back[10..].iter().all(|&b| b == 0));
}

#[test]
fn dup_shares_an_offset_and_separate_opens_do_not() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", b"abcdef");
    let one = ns.open(&ctx, None, b"/f", &READ, 0).unwrap();
    let dup = Arc::clone(&one);
    let other = ns.open(&ctx, None, b"/f", &READ, 0).unwrap();
    let mut buf = [0_u8; 3];
    let _ = one.read(&mut buf).unwrap();
    let _ = dup.read(&mut buf).unwrap();
    assert_eq!(&buf, b"def");
    let _ = other.read(&mut buf).unwrap();
    assert_eq!(&buf, b"abc");
}

#[test]
fn seek_measures_from_each_origin_and_refuses_before_the_start() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", b"0123456789");
    let file = ns.open(&ctx, None, b"/f", &READ, 0).unwrap();
    assert_eq!(file.seek(3, Whence::Set), Ok(3));
    assert_eq!(file.seek(2, Whence::Current), Ok(5));
    assert_eq!(file.seek(-1, Whence::End), Ok(9));
    assert_eq!(file.seek(-100, Whence::Current), Err(Errno::EINVAL));
    assert_eq!(file.seek(4, Whence::Data), Ok(4));
    assert_eq!(file.seek(4, Whence::Hole), Ok(10));
    assert_eq!(file.seek(10, Whence::Data), Err(Errno::ENXIO));
}

#[test]
fn an_unlinked_file_stays_readable_through_an_open_description() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/gone", b"still here");
    let file = ns.open(&ctx, None, b"/gone", &READ, 0).unwrap();
    ns.unlink(&ctx, None, b"/gone").unwrap();
    assert_eq!(read_file(&ns, &ctx, "/gone").unwrap_err(), Errno::ENOENT);
    let mut buf = [0_u8; 10];
    assert_eq!(file.read(&mut buf), Ok(10));
    assert_eq!(&buf, b"still here");
    assert_eq!(file.inode().metadata().nlink, 0);
}

#[test]
fn an_unlinked_or_replaced_file_is_freed_once_nothing_holds_it() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/doomed", b"x");
    write_file(&ns, &ctx, "/replaced", b"y");
    write_file(&ns, &ctx, "/replacement", b"z");
    let inode_of = |path: &str| {
        let file = ns.open(&ctx, None, path.as_bytes(), &READ, 0).unwrap();
        Arc::downgrade(file.inode())
    };
    let doomed = inode_of("/doomed");
    let replaced = inode_of("/replaced");
    for _ in 0..3 {
        let _ = read_file(&ns, &ctx, "/doomed");
        let _ = read_file(&ns, &ctx, "/replaced");
    }
    ns.unlink(&ctx, None, b"/doomed").unwrap();
    ns.rename(
        &ctx,
        (None, b"/replacement"),
        (None, b"/replaced"),
        RenameMode::Replace,
    )
    .unwrap();
    assert!(
        doomed.upgrade().is_none(),
        "the cache kept an unlinked file alive"
    );
    assert!(
        replaced.upgrade().is_none(),
        "the cache kept a replaced file alive"
    );
}

#[test]
fn a_removed_directory_is_freed_even_after_misses_inside_it() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    let directory = {
        let at = ns.resolve(&ctx, None, b"/d", true).unwrap();
        Arc::downgrade(&at.inode().unwrap())
    };
    for name in ["/d/missing", "/d/also-missing", "/d/missing/deeper"] {
        let _ = ns.resolve(&ctx, None, name.as_bytes(), true);
    }
    ns.rmdir(&ctx, None, b"/d").unwrap();
    assert!(
        directory.upgrade().is_none(),
        "cached misses inside a removed directory kept it alive"
    );
}

#[test]
fn a_rename_to_a_new_name_does_not_leave_its_miss_holding_the_directory() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    write_file(&ns, &ctx, "/d/a", b"x");
    let directory = {
        let at = ns.resolve(&ctx, None, b"/d", true).unwrap();
        Arc::downgrade(&at.inode().unwrap())
    };
    ns.rename(
        &ctx,
        (None, b"/d/a"),
        (None, b"/d/b"),
        RenameMode::NoReplace,
    )
    .unwrap();
    ns.unlink(&ctx, None, b"/d/b").unwrap();
    ns.rmdir(&ctx, None, b"/d").unwrap();
    assert!(
        directory.upgrade().is_none(),
        "the rename's destination miss kept the directory alive"
    );
}

#[test]
fn reading_a_directory_is_eisdir_and_writing_one_cannot_be_opened() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    let dir = ns.open(&ctx, None, b"/d", &READ, 0).unwrap();
    assert_eq!(dir.read(&mut [0; 4]), Err(Errno::EISDIR));
    let write = OpenFlags {
        write: true,
        ..OpenFlags::default()
    };
    assert_eq!(
        ns.open(&ctx, None, b"/d", &write, 0).unwrap_err(),
        Errno::EISDIR
    );
}

// -- Directories -------------------------------------------------------------

#[test]
fn a_missing_parent_is_enoent_and_a_file_on_the_way_is_enotdir() {
    let (ns, ctx) = fresh();
    assert_eq!(
        ns.mkdir(&ctx, None, b"/a/b", 0o755).unwrap_err(),
        Errno::ENOENT
    );
    write_file(&ns, &ctx, "/file", b"");
    assert_eq!(
        ns.mkdir(&ctx, None, b"/file/b", 0o755).unwrap_err(),
        Errno::ENOTDIR
    );
    assert_eq!(
        ns.resolve(&ctx, None, b"/file/", true).unwrap_err(),
        Errno::ENOTDIR
    );
}

#[test]
fn directory_link_counts_follow_their_subdirectories() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/d/one", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/d/two", 0o755).unwrap();
    write_file(&ns, &ctx, "/d/file", b"");
    let nlink = |path: &str| {
        let at = ns.resolve(&ctx, None, path.as_bytes(), true).unwrap();
        ns.stat(&at).unwrap().metadata.nlink
    };
    assert_eq!(nlink("/d"), 4);
    ns.rmdir(&ctx, None, b"/d/one").unwrap();
    assert_eq!(nlink("/d"), 3);
}

#[test]
fn rmdir_refuses_dot_dotdot_nonempty_and_files() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    write_file(&ns, &ctx, "/d/f", b"");
    assert_eq!(ns.rmdir(&ctx, None, b"/d").unwrap_err(), Errno::ENOTEMPTY);
    assert_eq!(ns.rmdir(&ctx, None, b"/d/.").unwrap_err(), Errno::EINVAL);
    assert_eq!(
        ns.rmdir(&ctx, None, b"/d/..").unwrap_err(),
        Errno::ENOTEMPTY
    );
    assert_eq!(ns.rmdir(&ctx, None, b"/d/f").unwrap_err(), Errno::ENOTDIR);
    assert_eq!(ns.unlink(&ctx, None, b"/d").unwrap_err(), Errno::EISDIR);
    assert_eq!(ns.rmdir(&ctx, None, b"/").unwrap_err(), Errno::EBUSY);
}

#[test]
fn readdir_lists_dot_dotdot_and_every_name_once() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    for name in ["c", "a", "b"] {
        write_file(&ns, &ctx, &alloc::format!("/d/{name}"), b"");
    }
    let dir = ns.open(&ctx, None, b"/d", &READ, 0).unwrap();
    assert_eq!(names(&dir), [".", "..", "c", "a", "b"]);
    // Read to the end: a second pass reports nothing until rewound.
    assert!(names(&dir).is_empty());
    let _ = dir.seek(0, Whence::Set).unwrap();
    assert_eq!(names(&dir).len(), 5);
}

#[test]
fn a_directory_being_emptied_while_read_loses_no_entries() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    for i in 0..50 {
        write_file(&ns, &ctx, &alloc::format!("/d/{i}"), b"");
    }
    let dir = ns.open(&ctx, None, b"/d", &READ, 0).unwrap();
    // `rm -rf`: read a few, delete them, read a few more.
    let mut removed = 0;
    loop {
        let mut batch = Vec::new();
        dir.read_dir(&mut |entry| {
            if batch.len() == 7 {
                return false;
            }
            batch.push(entry.name.to_vec());
            true
        })
        .unwrap();
        if batch.is_empty() {
            break;
        }
        for name in batch {
            if name != b"." && name != b".." {
                let mut path = b"/d/".to_vec();
                path.extend_from_slice(&name);
                ns.unlink(&ctx, None, &path).unwrap();
                removed += 1;
            }
        }
    }
    assert_eq!(removed, 50);
    ns.rmdir(&ctx, None, b"/d").unwrap();
}

#[test]
fn getdents_packing_resumes_where_a_full_buffer_stopped() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    let expected: Vec<Vec<u8>> = (0..40)
        .map(|i| alloc::format!("entry-{i:03}").into_bytes())
        .collect();
    for name in &expected {
        write_file(
            &ns,
            &ctx,
            core::str::from_utf8(&[b"/d/", &name[..]].concat()).unwrap(),
            b"",
        );
    }
    let dir = ns.open(&ctx, None, b"/d", &READ, 0).unwrap();
    let mut seen = Vec::new();
    loop {
        let mut buf = [0_u8; 100];
        let mut writer = DirentWriter::new(&mut buf);
        dir.read_dir(&mut |entry| {
            writer.push(entry.ino, entry.next, entry.kind.dirent_type(), entry.name)
        })
        .unwrap();
        let used = writer.used();
        if used == 0 {
            break;
        }
        for record in records(&buf[..used]) {
            seen.push(record.name.to_vec());
        }
    }
    assert_eq!(&seen[..2], &[b".".to_vec(), b"..".to_vec()]);
    assert_eq!(&seen[2..], &expected[..]);
}

#[test]
fn a_dirent_record_is_padded_to_eight_with_the_name_at_nineteen() {
    let mut buf = [0xFF_u8; 64];
    let mut writer = DirentWriter::new(&mut buf);
    assert!(writer.push(7, 3, 8, b"hello"));
    assert_eq!(writer.used(), 32);
    assert_eq!(&buf[19..24], b"hello");
    assert_eq!(buf[24], 0);
    assert_eq!(u16::from_le_bytes([buf[16], buf[17]]), 32);
    let mut tiny = [0_u8; 20];
    assert!(!DirentWriter::new(&mut tiny).push(1, 1, 8, b"x"));
}

// -- Symbolic links ----------------------------------------------------------

#[test]
fn relative_and_absolute_links_resolve_and_lstat_does_not_follow() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/usr", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/usr/bin", 0o755).unwrap();
    write_file(&ns, &ctx, "/usr/bin/busybox", b"#!");
    ns.symlink(&ctx, None, b"/bin", b"usr/bin").unwrap();
    ns.symlink(&ctx, None, b"/usr/bin/sh", b"/bin/busybox")
        .unwrap();
    assert_eq!(read_file(&ns, &ctx, "/bin/sh").unwrap(), b"#!");
    assert_eq!(kind(&ns, &ctx, "/bin", false), Ok(FileType::Symlink));
    assert_eq!(kind(&ns, &ctx, "/bin/", false), Ok(FileType::Directory));
    assert_eq!(
        ns.read_link(&ctx, None, b"/bin/sh").unwrap(),
        b"/bin/busybox"
    );
    assert_eq!(
        ns.read_link(&ctx, None, b"/usr").unwrap_err(),
        Errno::EINVAL
    );
}

#[test]
fn a_link_cycle_is_eloop_and_nofollow_refuses_a_link() {
    let (ns, ctx) = fresh();
    ns.symlink(&ctx, None, b"/a", b"b").unwrap();
    ns.symlink(&ctx, None, b"/b", b"a").unwrap();
    assert_eq!(read_file(&ns, &ctx, "/a").unwrap_err(), Errno::ELOOP);
    write_file(&ns, &ctx, "/real", b"");
    ns.symlink(&ctx, None, b"/link", b"real").unwrap();
    let nofollow = OpenFlags {
        nofollow: true,
        ..READ
    };
    assert_eq!(
        ns.open(&ctx, None, b"/link", &nofollow, 0).unwrap_err(),
        Errno::ELOOP
    );
}

#[test]
fn dotdot_through_a_link_is_lexical_to_the_target_not_the_link() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/x", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/x/y", 0o755).unwrap();
    write_file(&ns, &ctx, "/x/marker", b"x");
    ns.symlink(&ctx, None, b"/l", b"/x/y").unwrap();
    // `l/..` is `/x`, the target's parent, as on Linux.
    assert_eq!(read_file(&ns, &ctx, "/l/../marker").unwrap(), b"x");
}

// -- Hard links --------------------------------------------------------------

#[test]
fn hard_links_share_contents_and_count_names() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/one", b"shared");
    ns.link(&ctx, (None, b"/one"), false, (None, b"/two"))
        .unwrap();
    let at = ns.resolve(&ctx, None, b"/two", true).unwrap();
    assert_eq!(ns.stat(&at).unwrap().metadata.nlink, 2);
    ns.unlink(&ctx, None, b"/one").unwrap();
    assert_eq!(read_file(&ns, &ctx, "/two").unwrap(), b"shared");
    assert_eq!(ns.stat(&at).unwrap().metadata.nlink, 1);
    ns.mkdir(&ctx, None, b"/dir", 0o755).unwrap();
    assert_eq!(
        ns.link(&ctx, (None, b"/dir"), false, (None, b"/dir2"))
            .unwrap_err(),
        Errno::EPERM
    );
}

// -- Rename ------------------------------------------------------------------

#[test]
fn rename_replaces_a_file_and_moves_a_directory_with_its_contents() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/a", b"new");
    write_file(&ns, &ctx, "/b", b"old");
    ns.rename(&ctx, (None, b"/a"), (None, b"/b"), RenameMode::Replace)
        .unwrap();
    assert_eq!(read_file(&ns, &ctx, "/b").unwrap(), b"new");
    assert_eq!(read_file(&ns, &ctx, "/a").unwrap_err(), Errno::ENOENT);

    ns.mkdir(&ctx, None, b"/src", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/dst", 0o755).unwrap();
    write_file(&ns, &ctx, "/src/inner", b"moved");
    ns.rename(
        &ctx,
        (None, b"/src"),
        (None, b"/dst/sub"),
        RenameMode::Replace,
    )
    .unwrap();
    assert_eq!(read_file(&ns, &ctx, "/dst/sub/inner").unwrap(), b"moved");
    assert_eq!(kind(&ns, &ctx, "/src", true), Err(Errno::ENOENT));
}

#[test]
fn rename_refuses_the_cases_rename2_lists() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/d/sub", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/full", 0o755).unwrap();
    write_file(&ns, &ctx, "/full/f", b"");
    write_file(&ns, &ctx, "/file", b"");
    let r = |old: &[u8], new: &[u8], mode| ns.rename(&ctx, (None, old), (None, new), mode);
    assert_eq!(
        r(b"/d", b"/d/sub/inside", RenameMode::Replace),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        r(b"/d", b"/full", RenameMode::Replace),
        Err(Errno::ENOTEMPTY)
    );
    assert_eq!(r(b"/file", b"/d", RenameMode::Replace), Err(Errno::EISDIR));
    assert_eq!(
        r(b"/d/sub", b"/file", RenameMode::Replace),
        Err(Errno::ENOTDIR)
    );
    assert_eq!(
        r(b"/file", b"/full/f", RenameMode::NoReplace),
        Err(Errno::EEXIST)
    );
    assert_eq!(
        r(b"/d/sub", b"/d", RenameMode::Replace),
        Err(Errno::ENOTEMPTY)
    );
    assert_eq!(
        r(b"/missing", b"/x", RenameMode::Replace),
        Err(Errno::ENOENT)
    );
}

#[test]
fn a_working_directory_follows_its_directory_through_a_rename() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/a", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/a/b", 0o755).unwrap();
    let cwd = ns.resolve(&ctx, None, b"/a/b", true).unwrap();
    assert_eq!(ns.path_of(&cwd, &ctx.root), b"/a/b");
    ns.rename(
        &ctx,
        (None, b"/a"),
        (None, b"/renamed"),
        RenameMode::Replace,
    )
    .unwrap();
    assert_eq!(ns.path_of(&cwd, &ctx.root), b"/renamed/b");

    let here = Context {
        root: ctx.root.clone(),
        cwd,
        who: ctx.who.clone(),
        ns: None,
    };
    write_file(&ns, &here, "relative", b"r");
    assert_eq!(read_file(&ns, &ctx, "/renamed/b/relative").unwrap(), b"r");
    assert_eq!(read_file(&ns, &here, "../b/relative").unwrap(), b"r");
}

// -- The dentry cache --------------------------------------------------------

#[test]
fn a_cached_miss_does_not_hide_a_later_create() {
    let (ns, ctx) = fresh();
    for _ in 0..3 {
        assert_eq!(read_file(&ns, &ctx, "/later").unwrap_err(), Errno::ENOENT);
    }
    write_file(&ns, &ctx, "/later", b"here");
    assert_eq!(read_file(&ns, &ctx, "/later").unwrap(), b"here");
    ns.unlink(&ctx, None, b"/later").unwrap();
    assert_eq!(read_file(&ns, &ctx, "/later").unwrap_err(), Errno::ENOENT);
}

/// A chain of directories far deeper than a stack could drop one frame a
/// level goes on a small stack: the kernel's double-faulted on a tmpfs 200
/// deep at `umount` (ferrix-ea, 2026-09-26).
#[test]
fn a_deep_chain_of_dentries_drops_without_recursing() {
    // Miri does not hold a thread to its stack size, so under it the chain
    // checks the drops' aliasing only, and 300 levels do that in minutes.
    const DEPTH: usize = if cfg!(miri) { 300 } else { 20_000 };
    let dropped = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let (ns, ctx) = fresh();
            let mut at = ns.resolve(&ctx, None, b"/", true).unwrap();
            for _ in 0..DEPTH {
                ns.mkdir(&ctx, Some(&at), b"d", 0o755).unwrap();
                at = ns.resolve(&ctx, Some(&at), b"d", false).unwrap();
            }
            // The deepest location is the chain's only hold on most of it.
            drop(at);
            drop(ctx);
            drop(ns);
        })
        .unwrap()
        .join();
    assert!(dropped.is_ok(), "dropping the chain failed");
}

#[test]
fn the_cache_is_bounded() {
    let ns = Namespace::with_cache(tmpfs(1), 8, Arc::new(SpinParker));
    let ctx = ns.context();
    for i in 0..100 {
        let _ = ns.resolve(&ctx, None, alloc::format!("/miss-{i}").as_bytes(), true);
    }
    assert!(ns.cached() <= 8);
}

/// The cache's queue is made whole with the namespace and never grows: a
/// lookup that fills it allocates nothing for it, so a frame window around
/// lookups does not see the queue's buffer double (`Shared::cache`). The
/// default limit as the kernel uses it, filled twice over.
/// Twice the default cache's 4096 misses: past half an hour alone under Miri,
/// so left to the native run.
#[test]
#[cfg_attr(miri, ignore)]
fn the_cache_never_grows_its_queue() {
    let ns = Namespace::new(tmpfs(1), Arc::new(SpinParker));
    let made = ns.cache_capacity();
    assert!(made >= crate::DEFAULT_CACHE);
    let ctx = ns.context();
    for i in 0..2 * crate::DEFAULT_CACHE {
        let _ = ns.resolve(&ctx, None, alloc::format!("/miss-{i}").as_bytes(), true);
    }
    assert_eq!(ns.cached(), crate::DEFAULT_CACHE);
    assert_eq!(ns.cache_capacity(), made, "the queue's buffer grew");
}

/// A namespace with no cache keeps nothing and makes no queue.
#[test]
fn a_namespace_without_a_cache_keeps_nothing() {
    let ns = Namespace::with_cache(tmpfs(1), 0, Arc::new(SpinParker));
    let ctx = ns.context();
    for i in 0..16 {
        let _ = ns.resolve(&ctx, None, alloc::format!("/miss-{i}").as_bytes(), true);
    }
    assert_eq!(ns.cached(), 0);
    assert_eq!(ns.cache_capacity(), 0);
}

// -- Mounts ------------------------------------------------------------------

/// Write access, not creating: what `open(O_WRONLY)` asks.
const WRITE: OpenFlags = OpenFlags {
    read: false,
    write: true,
    create: false,
    exclusive: false,
    truncate: false,
    append: false,
    directory: false,
    nofollow: false,
    path: false,
    nonblock: false,
};

/// `/ro` mounted with `flags` over a tmpfs holding `file`, a directory
/// `dir`, a character device `null` and a pipe `fifo`, all made before the
/// flags were set.
fn flagged(flags: crate::MountFlags) -> (Namespace, Context) {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/ro", 0o755).unwrap();
    let at = ns.resolve(&ctx, None, b"/ro", true).unwrap();
    let _ = ns.mount(tmpfs(2), &at).unwrap();
    write_file(&ns, &ctx, "/ro/file", b"kept");
    ns.mkdir(&ctx, None, b"/ro/dir", 0o755).unwrap();
    let null = crate::NewNode::Device {
        kind: FileType::CharDevice,
        rdev: makedev(1, 3),
    };
    ns.mknod(&ctx, None, b"/ro/null", null, 0o666).unwrap();
    ns.mknod(&ctx, None, b"/ro/fifo", crate::NewNode::Fifo, 0o666)
        .unwrap();
    let root = ns.resolve(&ctx, None, b"/ro", true).unwrap();
    ns.remount(&root, flags).unwrap();
    (ns, ctx)
}

#[test]
fn a_read_only_mount_refuses_every_change_with_erofs() {
    let (ns, ctx) = flagged(crate::MountFlags::READ_ONLY);
    let refused = |what: Result<(), Errno>, why: &str| {
        assert_eq!(what.unwrap_err(), Errno::EROFS, "{why}");
    };
    refused(
        ns.open(&ctx, None, b"/ro/file", &WRITE, 0).map(drop),
        "write open",
    );
    refused(
        ns.open(&ctx, None, b"/ro/new", &RW_CREATE, 0o644).map(drop),
        "create",
    );
    refused(ns.mkdir(&ctx, None, b"/ro/d2", 0o755), "mkdir");
    refused(ns.symlink(&ctx, None, b"/ro/l", b"file"), "symlink");
    refused(ns.unlink(&ctx, None, b"/ro/file"), "unlink");
    refused(ns.unlink(&ctx, None, b"/ro/missing"), "unlink of nothing");
    refused(ns.rmdir(&ctx, None, b"/ro/dir"), "rmdir");
    refused(
        ns.rename(
            &ctx,
            (None, b"/ro/file"),
            (None, b"/ro/f2"),
            RenameMode::Replace,
        ),
        "rename",
    );
    refused(
        ns.link(&ctx, (None, b"/ro/file"), false, (None, b"/ro/f3")),
        "link",
    );
    let file = ns.resolve(&ctx, None, b"/ro/file", true).unwrap();
    refused(ns.truncate(&file, 0), "truncate");
    let change = crate::SetAttributes {
        permissions: Some(0o600),
        ..crate::SetAttributes::default()
    };
    refused(ns.set_attributes(&file, &change), "chmod");

    // What Linux still allows: an existing name is EEXIST first, reading
    // works, and a device or a pipe opens for writing -- they are not the
    // filesystem's to change.
    assert_eq!(
        ns.mkdir(&ctx, None, b"/ro/dir", 0o755).unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(read_file(&ns, &ctx, "/ro/file").unwrap(), b"kept");
    assert!(ns.open(&ctx, None, b"/ro/null", &WRITE, 0).is_ok());
    let fifo = OpenFlags {
        nonblock: true,
        read: true,
        ..WRITE
    };
    assert!(ns.open(&ctx, None, b"/ro/fifo", &fifo, 0).is_ok());

    // And a remount read-write gives it all back.
    let root = ns.resolve(&ctx, None, b"/ro", true).unwrap();
    ns.remount(&root, crate::MountFlags::NONE).unwrap();
    ns.mkdir(&ctx, None, b"/ro/d2", 0o755).unwrap();
    assert!(ns.open(&ctx, None, b"/ro/file", &WRITE, 0).is_ok());
}

#[test]
fn a_nodev_mount_refuses_its_devices_with_eacces() {
    let (ns, ctx) = flagged(crate::MountFlags::NODEV);
    assert_eq!(
        ns.open(&ctx, None, b"/ro/null", &READ, 0).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(read_file(&ns, &ctx, "/ro/file").unwrap(), b"kept");
    let path_only = OpenFlags {
        path: true,
        read: false,
        ..READ
    };
    assert!(ns.open(&ctx, None, b"/ro/null", &path_only, 0).is_ok());
}

#[test]
fn a_remount_acts_only_on_a_mount_root_and_shows_its_options() {
    let all = crate::MountFlags::READ_ONLY
        .union(crate::MountFlags::NOSUID)
        .union(crate::MountFlags::NODEV)
        .union(crate::MountFlags::NOEXEC)
        .union(crate::MountFlags::RELATIME);
    let (ns, ctx) = flagged(all);
    let inside = ns.resolve(&ctx, None, b"/ro/dir", true).unwrap();
    assert_eq!(
        ns.remount(&inside, crate::MountFlags::NONE).unwrap_err(),
        Errno::EINVAL
    );
    let root = ns.resolve(&ctx, None, b"/ro", true).unwrap();
    assert_eq!(root.mount.flags(), all);
    assert_eq!(
        root.mount.flags().options(),
        b"ro,nosuid,nodev,noexec,relatime"
    );
    assert_eq!(crate::MountFlags::NONE.options(), b"rw");
    {
        use ferrix_linux_abi::types::{
            ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RDONLY, ST_RELATIME, ST_VALID,
        };
        assert_eq!(
            root.mount.flags().statfs_flags(),
            ST_VALID | ST_RDONLY | ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME
        );
        assert_eq!(crate::MountFlags::NONE.statfs_flags(), ST_VALID);
    }
    assert!(root.mount.no_exec() && root.mount.no_set_id() && root.mount.no_devices());
}

#[test]
fn a_mount_point_is_shown_from_the_readers_root_or_not_at_all() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/jail", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/jail/mnt", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/other", 0o755).unwrap();
    let inside = ns.resolve(&ctx, None, b"/jail/mnt", true).unwrap();
    let outside = ns.resolve(&ctx, None, b"/other", true).unwrap();
    let a = ns.mount(tmpfs(2), &inside).unwrap();
    let b = ns.mount(tmpfs(3), &outside).unwrap();
    let jail = ns.resolve(&ctx, None, b"/jail", true).unwrap();
    let point = |mount: &Arc<crate::Mount>| Location {
        mount: Arc::clone(mount),
        dentry: Arc::clone(mount.root()),
    };
    assert_eq!(ns.path_within(&point(&a), &jail).unwrap(), b"/mnt");
    assert_eq!(ns.path_within(&point(&b), &jail), None);
    assert_eq!(ns.path_within(&point(&b), &ctx.root).unwrap(), b"/other");
    assert_eq!(ns.path_within(&jail, &jail).unwrap(), b"/");
    assert_eq!(Namespace::root_path(&a), b"/");
}

/// `/src` a tmpfs holding `f` and `a/deep`, with a second tmpfs on
/// `a/deep` holding `inner`; `/d1`, `/d2` empty directories and `/file` an
/// empty file on the root.
fn bind_fixture() -> (Namespace, Context) {
    let (ns, ctx) = fresh();
    for dir in ["/src", "/d1", "/d2"] {
        ns.mkdir(&ctx, None, dir.as_bytes(), 0o755).unwrap();
    }
    write_file(&ns, &ctx, "/file", b"");
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let _ = ns.mount(tmpfs(2), &src).unwrap();
    write_file(&ns, &ctx, "/src/f", b"bind");
    ns.mkdir(&ctx, None, b"/src/a", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/src/a/deep", 0o755).unwrap();
    let deep = ns.resolve(&ctx, None, b"/src/a/deep", true).unwrap();
    let _ = ns.mount(tmpfs(3), &deep).unwrap();
    write_file(&ns, &ctx, "/src/a/deep/inner", b"in");
    (ns, ctx)
}

#[test]
fn a_bind_shows_its_source_and_mrec_copies_what_is_mounted_below() {
    let (ns, ctx) = bind_fixture();
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let d1 = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let d2 = ns.resolve(&ctx, None, b"/d2", true).unwrap();
    let plain = ns.bind(&src, &d1, false).unwrap();
    assert_eq!(read_file(&ns, &ctx, "/d1/f").unwrap(), b"bind");
    assert_eq!(
        read_file(&ns, &ctx, "/d1/a/deep/inner").unwrap_err(),
        Errno::ENOENT
    );
    let _ = ns.bind(&src, &d2, true).unwrap();
    assert_eq!(read_file(&ns, &ctx, "/d2/a/deep/inner").unwrap(), b"in");
    // A write through one shows through the other: one filesystem.
    write_file(&ns, &ctx, "/d2/f", b"both");
    assert_eq!(read_file(&ns, &ctx, "/src/f").unwrap(), b"both");
    // Each bind is a mount of its own, with its own id, on its place.
    let d1_root = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    assert!(Arc::ptr_eq(&d1_root.mount, &plain));
    assert!(plain.shares_filesystem(&src.mount));
    assert_ne!(plain.id(), src.mount.id());
    assert_eq!(ns.path_of(&d1_root, &ctx.root), b"/d1");
    // `..` from the bind's root climbs to where it is mounted.
    let up = ns.resolve(&ctx, None, b"/d1/..", true).unwrap();
    assert!(up.same(&ctx.root));
}

#[test]
fn a_mount_point_reached_through_another_bind_can_be_neither_removed_nor_renamed() {
    let (ns, ctx) = bind_fixture();
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let d1 = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let _ = ns.bind(&src, &d1, false).unwrap();
    // `/d1/a/deep` is the submount's place, reached through a bind that
    // crosses no mount there.
    assert_eq!(
        ns.rmdir(&ctx, None, b"/d1/a/deep").unwrap_err(),
        Errno::EBUSY
    );
    assert_eq!(
        ns.rename(
            &ctx,
            (None, b"/d1/a/deep"),
            (None, b"/d1/a/moved"),
            RenameMode::Replace
        )
        .unwrap_err(),
        Errno::EBUSY
    );
    ns.mkdir(&ctx, None, b"/d1/a/other", 0o755).unwrap();
    assert_eq!(
        ns.rename(
            &ctx,
            (None, b"/d1/a/other"),
            (None, b"/d1/a/deep"),
            RenameMode::Replace
        )
        .unwrap_err(),
        Errno::EBUSY
    );
    assert_eq!(read_file(&ns, &ctx, "/src/a/deep/inner").unwrap(), b"in");
    // A file bound over a file is a mount point the same way.
    let f = ns.resolve(&ctx, None, b"/src/f", true).unwrap();
    let file = ns.resolve(&ctx, None, b"/file", true).unwrap();
    let _ = ns.bind(&f, &file, false).unwrap();
    let root = ns.resolve(&ctx, None, b"/", true).unwrap();
    let d2 = ns.resolve(&ctx, None, b"/d2", true).unwrap();
    let _ = ns.bind(&root, &d2, false).unwrap();
    assert_eq!(
        ns.unlink(&ctx, None, b"/d2/file").unwrap_err(),
        Errno::EBUSY
    );
}

#[test]
fn subtree_lists_a_mount_and_everything_inside_it_parents_first() {
    let (ns, ctx) = bind_fixture();
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let all = ns.subtree(&src).unwrap();
    assert_eq!(all.len(), 2);
    assert!(Arc::ptr_eq(&all[0], &src.mount));
    assert!(
        all[1]
            .parent()
            .is_some_and(|(above, _)| Arc::ptr_eq(&above, &src.mount))
    );
    let inside = ns.resolve(&ctx, None, b"/src/a", true).unwrap();
    assert_eq!(ns.subtree(&inside).unwrap_err(), Errno::EINVAL);
}

#[test]
fn a_subdirectory_and_a_file_bind_and_the_kinds_must_agree() {
    let (ns, ctx) = bind_fixture();
    let a = ns.resolve(&ctx, None, b"/src/a", true).unwrap();
    let f = ns.resolve(&ctx, None, b"/src/f", true).unwrap();
    let d1 = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let file = ns.resolve(&ctx, None, b"/file", true).unwrap();
    assert_eq!(ns.bind(&f, &d1, false).unwrap_err(), Errno::ENOTDIR);
    assert_eq!(ns.bind(&a, &file, false).unwrap_err(), Errno::ENOTDIR);
    let sub = ns.bind(&a, &d1, false).unwrap();
    assert_eq!(Namespace::root_path(&sub), b"/a");
    assert!(ns.resolve(&ctx, None, b"/d1/deep", true).is_ok());
    let one = ns.bind(&f, &file, false).unwrap();
    assert_eq!(Namespace::root_path(&one), b"/f");
    assert_eq!(read_file(&ns, &ctx, "/file").unwrap(), b"bind");
}

#[test]
fn a_bind_remount_reaches_one_mount_and_a_plain_one_every_bind() {
    let (ns, ctx) = bind_fixture();
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let d1 = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let _ = ns.bind(&src, &d1, true).unwrap();
    let d1 = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let writable = |path: &[u8]| ns.open(&ctx, None, path, &WRITE, 0).map(drop);

    ns.remount(&d1, crate::MountFlags::READ_ONLY).unwrap();
    assert_eq!(writable(b"/d1/f").unwrap_err(), Errno::EROFS);
    assert!(writable(b"/src/f").is_ok());
    ns.remount(&d1, crate::MountFlags::NONE).unwrap();

    ns.remount_filesystem(&d1, crate::MountFlags::READ_ONLY)
        .unwrap();
    assert_eq!(writable(b"/d1/f").unwrap_err(), Errno::EROFS);
    assert_eq!(writable(b"/src/f").unwrap_err(), Errno::EROFS);
    // Another filesystem, mounted inside, is not touched.
    assert!(writable(b"/d1/a/deep/inner").is_ok());
    assert!(src.mount.filesystem_read_only() && !src.mount.read_only());
    // A bind remount does not undo the filesystem's read-only; a plain one does.
    ns.remount(&d1, crate::MountFlags::NONE).unwrap();
    assert_eq!(writable(b"/src/f").unwrap_err(), Errno::EROFS);
    ns.remount_filesystem(&d1, crate::MountFlags::NONE).unwrap();
    assert!(writable(b"/src/f").is_ok() && writable(b"/d1/f").is_ok());
}

#[test]
fn detach_takes_the_subtree_and_leaves_its_top_without_a_parent() {
    let (ns, ctx) = bind_fixture();
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let d1 = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let _ = ns.bind(&src, &d1, true).unwrap();
    let kept = ns.resolve(&ctx, None, b"/d1/a/deep", true).unwrap();
    let top = ns.resolve(&ctx, None, b"/d1", true).unwrap();
    let before = ns.mounts().len();
    assert_eq!(ns.unmount(&top).unwrap_err(), Errno::EBUSY);
    let inside = ns.resolve(&ctx, None, b"/d1/a", true).unwrap();
    assert_eq!(ns.unmount_with(&inside, true).unwrap_err(), Errno::EINVAL);
    ns.unmount_with(&top, true).unwrap();
    assert_eq!(ns.mounts().len(), before - 2);
    assert_eq!(
        read_file(&ns, &ctx, "/d1/a/deep/inner").unwrap_err(),
        Errno::ENOENT
    );
    // What was held keeps working, and `..` stops at its mount's root.
    assert!(kept.mount.parent().is_none() && top.mount.parent().is_none());
    assert!(kept.parent().same(&kept));
    assert!(!ns.owns(&kept.mount));
    // Nothing of it binds, and it cannot be unmounted again.
    let d2 = ns.resolve(&ctx, None, b"/d2", true).unwrap();
    assert_eq!(ns.bind(&kept, &d2, false).unwrap_err(), Errno::EINVAL);
    assert_eq!(ns.unmount_with(&top, true).unwrap_err(), Errno::EINVAL);
    // The source's own mounts are untouched.
    assert_eq!(read_file(&ns, &ctx, "/src/a/deep/inner").unwrap(), b"in");
}

#[test]
fn a_directory_binds_onto_itself_and_a_recursive_bind_into_itself_ends() {
    let (ns, ctx) = bind_fixture();
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let _ = ns.bind(&src, &src, true).unwrap();
    assert_eq!(read_file(&ns, &ctx, "/src/a/deep/inner").unwrap(), b"in");
    // A bind of the tree into a directory of itself copies what was below
    // it once, not the copy too.
    let a = ns.resolve(&ctx, None, b"/src/a", true).unwrap();
    let whole = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let before = ns.mounts().len();
    let _ = ns.bind(&whole, &a, true).unwrap();
    assert_eq!(ns.mounts().len(), before + 2);
    assert_eq!(read_file(&ns, &ctx, "/src/a/a/deep/inner").unwrap(), b"in");
    for mount in ns.mounts() {
        let mut parent = mount.parent();
        for _ in 0..=64 {
            let Some((above, _)) = parent else { break };
            parent = above.parent();
        }
        assert!(parent.is_none(), "a mount's parents do not end");
    }
}

#[test]
fn a_mount_covers_a_directory_and_dotdot_climbs_out_of_it() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/mnt", 0o755).unwrap();
    write_file(&ns, &ctx, "/mnt/under", b"hidden");
    write_file(&ns, &ctx, "/top", b"top");
    let mnt = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    let mount = ns.mount(tmpfs(2), &mnt).unwrap();

    assert_eq!(
        read_file(&ns, &ctx, "/mnt/under").unwrap_err(),
        Errno::ENOENT
    );
    write_file(&ns, &ctx, "/mnt/over", b"over");
    let over = ns.resolve(&ctx, None, b"/mnt/over", true).unwrap();
    assert_eq!(ns.stat(&over).unwrap().dev, 2);
    assert_eq!(read_file(&ns, &ctx, "/mnt/../top").unwrap(), b"top");
    assert_eq!(ns.path_of(&over, &ctx.root), b"/mnt/over");

    assert_eq!(ns.rmdir(&ctx, None, b"/mnt").unwrap_err(), Errno::EBUSY);
    assert_eq!(
        ns.rename(
            &ctx,
            (None, b"/mnt/over"),
            (None, b"/moved"),
            RenameMode::Replace
        )
        .unwrap_err(),
        Errno::EXDEV
    );
    assert_eq!(
        ns.link(&ctx, (None, b"/top"), false, (None, b"/mnt/link"))
            .unwrap_err(),
        Errno::EXDEV
    );

    let root = ns.resolve(&ctx, None, b"/mnt", true).unwrap();
    assert!(Arc::ptr_eq(&root.mount, &mount));
    ns.unmount(&root).unwrap();
    assert_eq!(read_file(&ns, &ctx, "/mnt/under").unwrap(), b"hidden");
    assert_eq!(
        read_file(&ns, &ctx, "/mnt/over").unwrap_err(),
        Errno::ENOENT
    );
}

#[test]
fn unmount_takes_the_unmounted_tree_out_of_the_dentry_cache() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/m", 0o755).unwrap();
    let mut after_first = None;
    let mut last_inode = None;
    for cycle in 0..3_u64 {
        let at = ns.resolve(&ctx, None, b"/m", true).unwrap();
        let _ = ns.mount(tmpfs(10 + cycle), &at).unwrap();
        drop(at);
        ns.mkdir(&ctx, None, b"/m/d", 0o755).unwrap();
        write_file(&ns, &ctx, "/m/d/f", b"inside");
        let file = ns.resolve(&ctx, None, b"/m/d/f", true).unwrap();
        let inode = file.dentry.inode().unwrap();
        last_inode = Some(Arc::downgrade(&inode));
        drop((file, inode));
        let root = ns.resolve(&ctx, None, b"/m", true).unwrap();
        ns.unmount(&root).unwrap();
        drop(root);
        match after_first {
            None => after_first = Some(ns.cached()),
            Some(first) => assert_eq!(
                ns.cached(),
                first,
                "cycle {cycle} of mount and unmount left dentries in the cache"
            ),
        }
    }
    // The unmounted filesystem's file is gone with it: no cached dentry holds
    // its inode.
    assert!(
        last_inode.unwrap().upgrade().is_none(),
        "an unmounted file's inode outlived its mount"
    );
}

#[test]
fn unmount_refuses_a_mount_with_mounts_inside_it() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/a", 0o755).unwrap();
    let a = ns.resolve(&ctx, None, b"/a", true).unwrap();
    let _ = ns.mount(tmpfs(2), &a).unwrap();
    ns.mkdir(&ctx, None, b"/a/b", 0o755).unwrap();
    let b = ns.resolve(&ctx, None, b"/a/b", true).unwrap();
    let _ = ns.mount(tmpfs(3), &b).unwrap();
    let a_root = ns.resolve(&ctx, None, b"/a", true).unwrap();
    assert_eq!(ns.unmount(&a_root).unwrap_err(), Errno::EBUSY);
    assert_eq!(ns.unmount(&ctx.root).unwrap_err(), Errno::EBUSY);
    let b_root = ns.resolve(&ctx, None, b"/a/b", true).unwrap();
    ns.unmount(&b_root).unwrap();
    ns.unmount(&a_root).unwrap();
}

#[test]
fn dotdot_never_climbs_above_a_context_root() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/jail", 0o755).unwrap();
    write_file(&ns, &ctx, "/secret", b"s");
    write_file(&ns, &ctx, "/jail/secret", b"j");
    let jail = ns.resolve(&ctx, None, b"/jail", true).unwrap();
    let inside = Context {
        root: jail.clone(),
        cwd: jail,
        who: ctx.who,
        ns: None,
    };
    assert_eq!(read_file(&ns, &inside, "/../../secret").unwrap(), b"j");
}

// -- Mount namespaces (N3) ---------------------------------------------------

/// A copy of `ns`, with `ctx`'s root and working directory moved into it, and
/// a context naming it.
fn copied(ns: &Namespace, ctx: &Context) -> (Arc<Namespace>, Context) {
    let mut root = ctx.root.clone();
    let mut cwd = ctx.cwd.clone();
    let copy = Arc::new(ns.copy(&mut [&mut root, &mut cwd]).unwrap());
    let inside = Context {
        root,
        cwd,
        who: ctx.who.clone(),
        ns: Some(Arc::clone(&copy)),
    };
    (copy, inside)
}

/// A context whose root and working directory are both `at`.
fn rooted_at(ctx: &Context, at: &Location) -> Context {
    Context {
        root: at.clone(),
        cwd: at.clone(),
        ..ctx.clone()
    }
}

#[test]
fn a_copied_namespace_is_private_both_ways_and_shares_the_files() {
    let (ns, ctx) = bind_fixture();
    let (copy, inside) = copied(&ns, &ctx);
    assert_ne!(copy.id(), ns.id());
    assert_eq!(copy.mounts().len(), ns.mounts().len());
    // The copy's mounts are new ones, of the same filesystems.
    let src = ns.resolve(&ctx, None, b"/src", true).unwrap();
    let copied_src = copy.resolve(&inside, None, b"/src", true).unwrap();
    assert!(!Arc::ptr_eq(&src.mount, &copied_src.mount));
    assert!(src.mount.shares_filesystem(&copied_src.mount));
    assert!(copy.owns(&copied_src.mount) && !copy.owns(&src.mount));
    assert!(ns.owns(&src.mount) && !ns.owns(&copied_src.mount));
    assert_eq!(
        read_file(&copy, &inside, "/src/a/deep/inner").unwrap(),
        b"in"
    );
    // A mount in the copy is not in the first, and the reverse.
    let d1 = copy.resolve(&inside, None, b"/d1", true).unwrap();
    let _ = copy.mount(tmpfs(7), &d1).unwrap();
    write_file(&copy, &inside, "/d1/only-inside", b"i");
    assert_eq!(
        read_file(&ns, &ctx, "/d1/only-inside").unwrap_err(),
        Errno::ENOENT
    );
    let d2 = ns.resolve(&ctx, None, b"/d2", true).unwrap();
    let _ = ns.mount(tmpfs(8), &d2).unwrap();
    write_file(&ns, &ctx, "/d2/only-outside", b"o");
    assert_eq!(
        read_file(&copy, &inside, "/d2/only-outside").unwrap_err(),
        Errno::ENOENT
    );
    // An unmount in one leaves the other's copy where it was.
    let copied_deep = copy.resolve(&inside, None, b"/src/a/deep", true).unwrap();
    copy.unmount(&copied_deep).unwrap();
    assert_eq!(read_file(&ns, &ctx, "/src/a/deep/inner").unwrap(), b"in");
    // A file written through a shared filesystem shows in both.
    write_file(&copy, &inside, "/src/f", b"shared");
    assert_eq!(read_file(&ns, &ctx, "/src/f").unwrap(), b"shared");
    // A mount of the other namespace is refused as the place of a change.
    assert_eq!(copy.bind(&src, &d1, false).unwrap_err(), Errno::EINVAL);
}

#[test]
fn a_location_crosses_the_mounts_of_the_namespace_it_was_reached_in() {
    let (ns, ctx) = bind_fixture();
    let before = ns.resolve(&ctx, None, b"/src/a", true).unwrap();
    let (copy, inside) = copied(&ns, &ctx);
    let copied_deep = copy.resolve(&inside, None, b"/src/a/deep", true).unwrap();
    copy.unmount(&copied_deep).unwrap();
    // Walked from a place kept from before the copy, through the copy's
    // methods, `deep` still crosses the first namespace's mount: the table
    // is the mount's, not the walker's.
    let from_before = Context {
        cwd: before,
        ..inside.clone()
    };
    assert_eq!(read_file(&copy, &from_before, "deep/inner").unwrap(), b"in");
    assert_eq!(
        read_file(&copy, &inside, "/src/a/deep/inner").unwrap_err(),
        Errno::ENOENT
    );
}

#[test]
fn a_copy_moves_the_places_it_is_given_and_leaves_others() {
    let (ns, ctx) = bind_fixture();
    let mut cwd = ns.resolve(&ctx, None, b"/src/a", true).unwrap();
    let kept = cwd.clone();
    let copy = ns.copy(&mut [&mut cwd]).unwrap();
    assert!(copy.owns(&cwd.mount) && !Arc::ptr_eq(&cwd.mount, &kept.mount));
    assert!(Arc::ptr_eq(&cwd.dentry, &kept.dentry));
    let fs = tmpfs(9);
    let pipe = Location::detached(
        Arc::clone(&fs),
        fs.root(),
        b"pipe:[1]",
        Arc::new(SpinParker),
    )
    .unwrap();
    let mut outside = pipe.clone();
    let _ = ns.copy(&mut [&mut outside]).unwrap();
    assert!(outside.same(&pipe));
}

#[test]
fn an_ended_namespace_gives_its_mount_points_back() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/m", 0o755).unwrap();
    let (copy, inside) = copied(&ns, &ctx);
    let m = copy.resolve(&inside, None, b"/m", true).unwrap();
    let mounted = copy.mount(tmpfs(2), &m).unwrap();
    write_file(&copy, &inside, "/m/x", b"x");
    // The dentry is a mount point in the copy, so the first may not remove it.
    assert_eq!(ns.rmdir(&ctx, None, b"/m").unwrap_err(), Errno::EBUSY);
    drop((copy, inside, m));
    // Held on, the mount is disconnected: nothing above it.
    assert!(mounted.parent().is_none());
    ns.rmdir(&ctx, None, b"/m").unwrap();
}

/// 4096 binds in one directory, each one looked up among those before it.
/// Slow under Miri, past CI's six hours, so left to the native run.
#[test]
#[cfg_attr(miri, ignore)]
fn a_namespace_holds_mount_max_mounts_and_refuses_the_next_with_enospc() {
    let (ns, ctx) = fresh();
    let root = ns.resolve(&ctx, None, b"/", true).unwrap();
    let mut index = 0;
    let refused = loop {
        let name = alloc::format!("/m{index}");
        index += 1;
        ns.mkdir(&ctx, None, name.as_bytes(), 0o755).unwrap();
        let at = ns.resolve(&ctx, None, name.as_bytes(), true).unwrap();
        if let Err(errno) = ns.bind(&root, &at, false) {
            break errno;
        }
        assert!(ns.mounts().len() <= MOUNT_MAX);
    };
    assert_eq!(refused, Errno::ENOSPC);
    assert_eq!(ns.mounts().len(), MOUNT_MAX);
    // A copy of a full namespace is as full, and no more.
    let copy = ns.copy(&mut []).unwrap();
    assert_eq!(copy.mounts().len(), MOUNT_MAX);
}

/// A namespace whose context's root is a mount with a parent, as a process's
/// is after the root switch: `/sysroot`, a tmpfs, with `/new` in it a second
/// one to pivot to, and `/new/old` for the old root.
fn pivot_fixture() -> (Namespace, Context) {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/first-root-only", b"f");
    ns.mkdir(&ctx, None, b"/sysroot", 0o755).unwrap();
    let at = ns.resolve(&ctx, None, b"/sysroot", true).unwrap();
    let _ = ns.mount(tmpfs(2), &at).unwrap();
    let sysroot = ns.resolve(&ctx, None, b"/sysroot", true).unwrap();
    let ctx = rooted_at(&ctx, &sysroot);
    write_file(&ns, &ctx, "/old-root-only", b"o");
    ns.mkdir(&ctx, None, b"/new", 0o755).unwrap();
    let new = ns.resolve(&ctx, None, b"/new", true).unwrap();
    let _ = ns.mount(tmpfs(3), &new).unwrap();
    write_file(&ns, &ctx, "/new/new-root-only", b"n");
    ns.mkdir(&ctx, None, b"/new/old", 0o755).unwrap();
    (ns, ctx)
}

#[test]
fn pivot_root_swaps_the_root_and_puts_the_old_one_where_asked() {
    let (ns, ctx) = pivot_fixture();
    let (copy, inside) = copied(&ns, &ctx);
    let new = copy.resolve(&inside, None, b"/new", true).unwrap();
    let old = copy.resolve(&inside, None, b"/new/old", true).unwrap();
    copy.pivot_root(&inside.root, &new, &old).unwrap();
    // The caller moves its own root, as `chroot_fs_refs` does.
    let pivoted = rooted_at(&inside, &new);
    assert_eq!(read_file(&copy, &pivoted, "/new-root-only").unwrap(), b"n");
    assert_eq!(
        read_file(&copy, &pivoted, "/old/old-root-only").unwrap(),
        b"o"
    );
    // `..` from the new root's top stops there, and from the old root climbs
    // back into the new one.
    let up = copy.resolve(&pivoted, None, b"/old/..", true).unwrap();
    assert!(up.same(&new));
    // The old root is mounted where `put_old` was, and leaves with a detach.
    let old_root = copy.resolve(&pivoted, None, b"/old", true).unwrap();
    assert!(old_root.is_mount_root());
    copy.unmount_with(&old_root, true).unwrap();
    assert_eq!(
        read_file(&copy, &pivoted, "/old/old-root-only").unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        read_file(&copy, &pivoted, "/../../first-root-only").unwrap_err(),
        Errno::ENOENT
    );
    // The first namespace is as it was.
    assert_eq!(read_file(&ns, &ctx, "/new/new-root-only").unwrap(), b"n");
    assert_eq!(read_file(&ns, &ctx, "/old-root-only").unwrap(), b"o");
}

#[test]
fn pivot_root_dot_dot_stacks_the_old_root_on_the_new_one() {
    let (ns, ctx) = pivot_fixture();
    let (copy, inside) = copied(&ns, &ctx);
    let new = copy.resolve(&inside, None, b"/new", true).unwrap();
    copy.pivot_root(&inside.root, &new, &new).unwrap();
    let pivoted = rooted_at(&inside, &new);
    // `/` is the new root, and the old one is on top of it, where only
    // `umount2(".")` reaches it: a walk of `.` stays, a crossing finds it.
    assert_eq!(read_file(&copy, &pivoted, "/new-root-only").unwrap(), b"n");
    let stacked = copy.descend_mounts(new.clone());
    assert!(!stacked.same(&new));
    let on_top = rooted_at(&pivoted, &stacked);
    assert_eq!(read_file(&copy, &on_top, "/old-root-only").unwrap(), b"o");
    copy.unmount_with(&stacked, true).unwrap();
    assert!(copy.descend_mounts(new.clone()).same(&new));
    assert_eq!(read_file(&copy, &pivoted, "/new-root-only").unwrap(), b"n");
    assert_eq!(
        read_file(&copy, &pivoted, "/../old-root-only").unwrap_err(),
        Errno::ENOENT
    );
}

#[test]
fn pivot_root_refuses_what_linux_refuses_and_changes_nothing() {
    let (ns, ctx) = pivot_fixture();
    let (copy, inside) = copied(&ns, &ctx);
    let new = copy.resolve(&inside, None, b"/new", true).unwrap();
    let old = copy.resolve(&inside, None, b"/new/old", true).unwrap();
    let root = inside.root.clone();
    // On the caller's own root mount: `EBUSY`.
    copy.mkdir(&inside, None, b"/plain", 0o755).unwrap();
    let plain = copy.resolve(&inside, None, b"/plain", true).unwrap();
    assert_eq!(
        copy.pivot_root(&root, &plain, &old).unwrap_err(),
        Errno::EBUSY
    );
    assert_eq!(
        copy.pivot_root(&root, &new, &plain).unwrap_err(),
        Errno::EBUSY
    );
    // `new_root` not a mount's root.
    let other = copy.resolve(&inside, None, b"/plain", true).unwrap();
    let _ = copy.mount(tmpfs(4), &other).unwrap();
    copy.mkdir(&inside, None, b"/plain/sub", 0o755).unwrap();
    let sub = copy.resolve(&inside, None, b"/plain/sub", true).unwrap();
    assert_eq!(
        copy.pivot_root(&root, &sub, &sub).unwrap_err(),
        Errno::EINVAL
    );
    // `put_old` not below `new_root`.
    assert_eq!(
        copy.pivot_root(&root, &new, &sub).unwrap_err(),
        Errno::EINVAL
    );
    // A root with no parent: only the bottom mount, which no program's root
    // is.
    assert_eq!(
        copy.pivot_root(&copy.bottom(), &new, &old).unwrap_err(),
        Errno::EINVAL
    );
    // A mount of another namespace.
    let theirs = ns.resolve(&ctx, None, b"/new", true).unwrap();
    assert_eq!(
        copy.pivot_root(&root, &theirs, &old).unwrap_err(),
        Errno::EINVAL
    );
    // Not a directory.
    write_file(&copy, &inside, "/new/file", b"");
    let file = copy.resolve(&inside, None, b"/new/file", true).unwrap();
    assert_eq!(
        copy.pivot_root(&root, &new, &file).unwrap_err(),
        Errno::ENOTDIR
    );
    // The refusals changed nothing, and the pivot then goes through.
    assert_eq!(
        read_file(&copy, &inside, "/new/new-root-only").unwrap(),
        b"n"
    );
    copy.pivot_root(&root, &new, &old).unwrap();
    let pivoted = rooted_at(&inside, &new);
    assert_eq!(
        read_file(&copy, &pivoted, "/old/plain/sub/../../old-root-only").unwrap(),
        b"o"
    );
}

#[test]
fn slash_is_a_mount_on_an_empty_bottom_and_pivots_from_memory() {
    let (ns, ctx) = fresh();
    // `/` is a mount with a parent, and the bottom under it holds nothing.
    let root = ns.root();
    assert!(root.is_mount_root());
    let (below, _) = root.mount.parent().expect("/ is on the bottom mount");
    assert!(below.parent().is_none());
    assert!(
        root.parent()
            .dentry
            .inode()
            .unwrap()
            .lookup(b"anything")
            .is_err()
    );
    assert!(below.read_only());
    // So a copy's `/`, a tmpfs in memory, pivots as a disk root would.
    write_file(&ns, &ctx, "/old-only", b"o");
    ns.mkdir(&ctx, None, b"/new", 0o755).unwrap();
    let new = ns.resolve(&ctx, None, b"/new", true).unwrap();
    let _ = ns.mount(tmpfs(2), &new).unwrap();
    ns.mkdir(&ctx, None, b"/new/old", 0o755).unwrap();
    let (copy, inside) = copied(&ns, &ctx);
    let new = copy.resolve(&inside, None, b"/new", true).unwrap();
    let old = copy.resolve(&inside, None, b"/new/old", true).unwrap();
    copy.pivot_root(&inside.root, &new, &old).unwrap();
    // The namespace's `/` for a new process follows the pivot.
    assert!(copy.root().same(&new));
    let pivoted = rooted_at(&inside, &new);
    assert_eq!(read_file(&copy, &pivoted, "/old/old-only").unwrap(), b"o");
    let old_root = copy.resolve(&pivoted, None, b"/old", true).unwrap();
    copy.unmount_with(&old_root, true).unwrap();
    assert_eq!(
        read_file(&copy, &pivoted, "/old/old-only").unwrap_err(),
        Errno::ENOENT
    );
    // The first namespace's `/` is where it was.
    assert_eq!(read_file(&ns, &ctx, "/old-only").unwrap(), b"o");
}

#[test]
fn slash_cannot_be_unmounted() {
    let (ns, ctx) = fresh();
    assert_eq!(ns.unmount(&ctx.root).unwrap_err(), Errno::EBUSY);
    assert_eq!(ns.unmount_with(&ctx.root, true).unwrap_err(), Errno::EBUSY);
    assert_eq!(ns.unmount(&ns.bottom()).unwrap_err(), Errno::EINVAL);
    assert!(ns.root().same(&ctx.root));
}

// -- openat2's resolve flags ---------------------------------------------------

/// Open `path` from `dir` as `openat2` with `resolve` would: for `in_root`,
/// a context whose root is `dir`.
fn open_resolving(
    ns: &Namespace,
    ctx: &Context,
    dir: &[u8],
    path: &[u8],
    resolve: crate::Resolve,
    in_root: bool,
) -> Result<Vec<u8>, Errno> {
    let dir = ns.resolve(ctx, None, dir, true)?;
    let ctx = if in_root {
        rooted_at(ctx, &dir)
    } else {
        ctx.clone()
    };
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let file = ns.open_resolving(&ctx, Some(&dir), path, &flags, 0, resolve)?;
    let mut buf = vec![0_u8; 64];
    let len = file.read(&mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

/// A tree to resolve in: `/jail` holding a file, an absolute link and a
/// relative one out of it, and a tmpfs mounted at `/jail/m`.
fn resolve_fixture() -> (Namespace, Context) {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/secret", b"outside");
    ns.mkdir(&ctx, None, b"/jail", 0o755).unwrap();
    write_file(&ns, &ctx, "/jail/secret", b"inside");
    ns.symlink(&ctx, None, b"/jail/abs", b"/secret").unwrap();
    ns.symlink(&ctx, None, b"/jail/rel", b"../secret").unwrap();
    ns.mkdir(&ctx, None, b"/jail/m", 0o755).unwrap();
    let m = ns.resolve(&ctx, None, b"/jail/m", true).unwrap();
    let _ = ns.mount(tmpfs(2), &m).unwrap();
    write_file(&ns, &ctx, "/jail/m/f", b"mounted");
    (ns, ctx)
}

#[test]
fn resolve_in_root_keeps_every_path_and_link_inside_the_directory() {
    let (ns, ctx) = resolve_fixture();
    let in_root = crate::Resolve::default();
    for path in [&b"/secret"[..], b"abs", b"rel", b"../../secret"] {
        assert_eq!(
            open_resolving(&ns, &ctx, b"/jail", path, in_root, true).unwrap(),
            b"inside",
            "{}",
            String::from_utf8_lossy(path)
        );
    }
    // Without it, the links lead out.
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail", b"abs", in_root, false).unwrap(),
        b"outside"
    );
}

#[test]
fn resolve_beneath_refuses_what_in_root_would_clamp() {
    let (ns, ctx) = resolve_fixture();
    let beneath = crate::Resolve {
        beneath: true,
        ..crate::Resolve::default()
    };
    for path in [&b"/secret"[..], b"abs", b"rel", b"../secret"] {
        assert_eq!(
            open_resolving(&ns, &ctx, b"/jail", path, beneath, true).unwrap_err(),
            Errno::EXDEV,
            "{}",
            String::from_utf8_lossy(path)
        );
    }
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail", b"m/../secret", beneath, true).unwrap(),
        b"inside"
    );
}

#[test]
fn resolve_no_symlinks_and_no_xdev_refuse_their_steps() {
    let (ns, ctx) = resolve_fixture();
    let no_symlinks = crate::Resolve {
        no_symlinks: true,
        ..crate::Resolve::default()
    };
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail", b"rel", no_symlinks, false).unwrap_err(),
        Errno::ELOOP
    );
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail", b"secret", no_symlinks, false).unwrap(),
        b"inside"
    );
    let no_xdev = crate::Resolve {
        no_xdev: true,
        ..crate::Resolve::default()
    };
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail", b"m/f", no_xdev, false).unwrap_err(),
        Errno::EXDEV
    );
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail/m", b"../secret", no_xdev, false).unwrap_err(),
        Errno::EXDEV
    );
    assert_eq!(
        open_resolving(&ns, &ctx, b"/jail", b"secret", no_xdev, false).unwrap(),
        b"inside"
    );
}

// -- Permissions -------------------------------------------------------------

fn as_user(ctx: &Context, uid: u32) -> Context {
    Context {
        who: Access::user(uid, uid),
        ..ctx.clone()
    }
}

#[test]
fn a_user_cannot_read_write_or_search_what_the_mode_withholds() {
    let (ns, root) = fresh();
    write_file(&ns, &root, "/secret", b"s");
    ns.mkdir(&root, None, b"/private", 0o700).unwrap();
    write_file(&ns, &root, "/private/inside", b"i");
    let user = as_user(&root, 1000);

    assert_eq!(read_file(&ns, &user, "/secret").unwrap(), b"s");
    assert_eq!(
        ns.open(&user, None, b"/secret", &RW_CREATE, 0o644)
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        read_file(&ns, &user, "/private/inside").unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        ns.resolve(&user, None, b"/private/inside", true)
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        ns.mkdir(&user, None, b"/mine", 0o755).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        ns.unlink(&user, None, b"/secret").unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(read_file(&ns, &root, "/private/inside").unwrap(), b"i");
}

#[test]
fn a_sticky_directory_keeps_users_to_their_own_files() {
    let (ns, root) = fresh();
    ns.mkdir(&root, None, b"/tmp", 0o1777).unwrap();
    let alice = as_user(&root, 1000);
    let bob = as_user(&root, 1001);
    write_file(&ns, &alice, "/tmp/alice", b"a");
    let at = ns.resolve(&root, None, b"/tmp/alice", true).unwrap();
    let made = ns.stat(&at).unwrap().metadata;
    assert_eq!((made.uid, made.gid), (1000, 1000), "the creator owns it");

    assert_eq!(
        ns.unlink(&bob, None, b"/tmp/alice").unwrap_err(),
        Errno::EPERM
    );
    assert_eq!(
        ns.rename(
            &bob,
            (None, b"/tmp/alice"),
            (None, b"/tmp/bob"),
            RenameMode::Replace
        )
        .unwrap_err(),
        Errno::EPERM
    );
    write_file(&ns, &alice, "/tmp/alice", b"again");
    ns.unlink(&alice, None, b"/tmp/alice").unwrap();
}

#[test]
fn moving_a_directory_to_another_parent_needs_write_on_it() {
    let (ns, root) = fresh();
    ns.mkdir(&root, None, b"/tmp", 0o1777).unwrap();
    let user = as_user(&root, 1000);
    ns.mkdir(&user, None, b"/tmp/a", 0o755).unwrap();
    ns.mkdir(&user, None, b"/tmp/a/sub", 0o555).unwrap();
    ns.mkdir(&user, None, b"/tmp/b", 0o755).unwrap();
    assert_eq!(
        ns.rename(
            &user,
            (None, b"/tmp/a/sub"),
            (None, b"/tmp/b/sub"),
            RenameMode::Replace
        )
        .unwrap_err(),
        Errno::EACCES
    );
    ns.rename(
        &user,
        (None, b"/tmp/a/sub"),
        (None, b"/tmp/a/renamed"),
        RenameMode::Replace,
    )
    .unwrap();
}

// -- Descriptor tables -------------------------------------------------------

#[test]
fn descriptors_are_the_lowest_free_number() {
    let mut table = FdTable::new();
    assert_eq!(table.insert('a', false), Ok(0));
    assert_eq!(table.insert('b', false), Ok(1));
    assert_eq!(table.insert('c', false), Ok(2));
    assert_eq!(table.remove(1), Ok('b'));
    assert_eq!(table.insert('d', false), Ok(1));
    assert_eq!(table.insert_from(5, 'e', false), Ok(5));
    assert_eq!(table.insert('f', false), Ok(3));
    assert_eq!(table.len(), 5);
}

#[test]
fn dup2_replaces_and_hands_back_the_old_description() {
    let mut table = FdTable::new();
    let _ = table.insert("stdout", false).unwrap();
    let _ = table.insert("file", false).unwrap();
    assert_eq!(table.install(0, "file", false), Ok(Some("stdout")));
    assert_eq!(table.install(7, "x", true), Ok(None));
    assert_eq!(table.get(7), Ok(&"x"));
    assert_eq!(table.install(-1, "x", false), Err(Errno::EBADF));
    assert_eq!(table.install(1024, "x", false), Err(Errno::EBADF));
    assert_eq!(table.get(3), Err(Errno::EBADF));
    assert_eq!(table.get(-1), Err(Errno::EBADF));
}

#[test]
fn a_full_table_is_emfile_and_exec_closes_cloexec() {
    let mut table = FdTable::new();
    table.set_limit(4).unwrap();
    for i in 0..4 {
        let _ = table.insert(i, i % 2 == 1).unwrap();
    }
    assert_eq!(table.insert(9, false), Err(Errno::EMFILE));
    assert_eq!(table.insert_from(4, 9, false), Err(Errno::EINVAL));
    let mut closed = table.take_cloexec();
    closed.sort_unstable();
    assert_eq!(closed, [1, 3]);
    assert_eq!(table.iter().map(|(fd, _)| fd).collect::<Vec<_>>(), [0, 2]);
    assert_eq!(table.set_limit(2_000_000), Err(Errno::EPERM));
}

#[test]
fn a_reserved_descriptor_is_taken_names_nothing_and_is_filled_once() {
    let mut table = FdTable::new();
    let _ = table.insert('a', false).unwrap();
    let reserved = table.reserve(true).unwrap();
    assert_eq!(reserved.fd(), 1);
    // Taken: the next number handed out is past it...
    assert_eq!(table.insert('b', false), Ok(2));
    // ...but it names nothing yet, as on Linux.
    assert_eq!(table.get(1), Err(Errno::EBADF));
    assert_eq!(table.remove(1), Err(Errno::EBADF));
    assert_eq!(table.install(1, 'x', false), Err(Errno::EBUSY));
    assert_eq!(table.iter().map(|(fd, _)| fd).collect::<Vec<_>>(), [0, 2]);

    // A fork now copies the descriptors and not the reservation, which
    // nothing would ever fill in the child.
    let child = table.try_clone().unwrap();
    assert_eq!(child.len(), 2);

    assert_eq!(table.fill(reserved, 'c'), Ok(1));
    assert_eq!(table.get(1), Ok(&'c'));
    assert_eq!(
        table.cloexec(1),
        Ok(true),
        "the reservation's close-on-exec"
    );
    assert_eq!(table.len(), 3);

    // And a reservation spent on a table it did not come from hands the item
    // back rather than dropping it.
    let mut other: FdTable<char> = FdTable::new();
    let stray = table.reserve(false).unwrap();
    assert_eq!(other.fill(stray, 'z'), Err('z'));
}

#[test]
fn a_released_reservation_frees_its_number_and_a_full_table_reserves_nothing() {
    let mut table = FdTable::new();
    table.set_limit(2).unwrap();
    let _ = table.insert(0, false).unwrap();
    let reserved = table.reserve(false).unwrap();
    assert_eq!(table.reserve(false), Err(Errno::EMFILE));
    assert_eq!(table.insert(9, false), Err(Errno::EMFILE));
    table.release(reserved);
    assert_eq!(table.len(), 1);
    assert_eq!(table.insert(1, false), Ok(1));
}

// -- initramfs ---------------------------------------------------------------

/// Build a newc archive.
struct Newc(Vec<u8>);

impl Newc {
    fn new() -> Newc {
        Newc(Vec::new())
    }

    fn pad(&mut self) {
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
    }

    fn entry(
        &mut self,
        name: &str,
        mode: u32,
        ino: u32,
        nlink: u32,
        rdev: (u32, u32),
        data: &[u8],
    ) {
        let fields = [
            ino,
            mode,
            0,
            0,
            nlink,
            1_700_000_000,
            u32::try_from(data.len()).unwrap(),
            0,
            0,
            rdev.0,
            rdev.1,
            u32::try_from(name.len() + 1).unwrap(),
            0,
        ];
        self.0.extend_from_slice(b"070701");
        for field in fields {
            self.0
                .extend_from_slice(alloc::format!("{field:08X}").as_bytes());
        }
        self.0.extend_from_slice(name.as_bytes());
        self.0.push(0);
        self.pad();
        self.0.extend_from_slice(data);
        self.pad();
    }

    fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, 0, 1, (0, 0), b"");
        self.0
    }
}

#[test]
fn an_archive_unpacks_with_links_nodes_and_unsafe_names_skipped() {
    let mut archive = Newc::new();
    archive.entry(".", 0o040_700, 1, 2, (0, 0), b"");
    archive.entry("bin", 0o040_755, 2, 2, (0, 0), b"");
    archive.entry("bin/busybox", 0o100_755, 3, 1, (0, 0), b"\x7fELF");
    archive.entry("bin/sh", 0o120_777, 4, 1, (0, 0), b"busybox");
    archive.entry("etc", 0o040_755, 5, 2, (0, 0), b"");
    archive.entry("etc/a", 0o100_644, 6, 2, (0, 0), b"");
    archive.entry("etc/b", 0o100_644, 6, 2, (0, 0), b"linked");
    archive.entry("dev", 0o040_755, 7, 2, (0, 0), b"");
    archive.entry("dev/console", 0o020_600, 8, 1, (5, 1), b"");
    archive.entry("../escape", 0o100_644, 9, 1, (0, 0), b"no");
    archive.entry("./etc/", 0o040_750, 5, 2, (0, 0), b"");
    let archive = archive.finish();

    let (ns, ctx) = fresh();
    let made = initramfs::unpack(&ns, &ctx, &archive).unwrap();
    assert_eq!(made.directories, 3);
    assert_eq!(made.files, 2);
    assert_eq!(made.symlinks, 1);
    assert_eq!(made.hard_links, 1);
    assert_eq!(made.nodes, 1);
    assert_eq!(made.skipped, 1);

    assert_eq!(read_file(&ns, &ctx, "/bin/sh").unwrap(), b"\x7fELF");
    assert_eq!(read_file(&ns, &ctx, "/etc/a").unwrap(), b"linked");
    let console = ns.resolve(&ctx, None, b"/dev/console", true).unwrap();
    let meta = ns.stat(&console).unwrap().metadata;
    assert_eq!(meta.kind, FileType::CharDevice);
    assert_eq!(meta.rdev, makedev(5, 1));
    assert_eq!(meta.rdev, 0x501);

    let etc = ns.resolve(&ctx, None, b"/etc", true).unwrap();
    let meta = ns.stat(&etc).unwrap().metadata;
    assert_eq!(meta.permissions, 0o750);
    assert_eq!(meta.mtime.tv_sec, 1_700_000_000);
    assert_eq!(ns.stat(&ctx.root).unwrap().metadata.permissions, 0o700);
}

/// The evidence of SAFETY-MANUAL AoU-24: the unpacker reads pid 1's inputs
/// and creates none of them.
#[test]
fn pid_1s_inputs_are_read_from_the_archive_and_never_unpacked() {
    let mut archive = Newc::new();
    archive.entry(".", 0o040_700, 1, 2, (0, 0), b"");
    archive.entry("./.ferrix/", 0o040_755, 2, 2, (0, 0), b"");
    archive.entry(".ferrix/init", 0o040_755, 3, 2, (0, 0), b"");
    archive.entry(
        "./.ferrix/init/program",
        0o100_755,
        4,
        1,
        (0, 0),
        b"\x7fELF",
    );
    archive.entry(".ferrix/init/script", 0o100_644, 5, 1, (0, 0), b"echo hi");
    archive.entry("bin", 0o040_755, 6, 2, (0, 0), b"");
    archive.entry("bin/true", 0o100_755, 7, 1, (0, 0), b"t");
    // Not beneath it: a name that only starts the same way is unpacked.
    archive.entry(".ferrixish", 0o100_644, 8, 1, (0, 0), b"x");
    let archive = archive.finish();

    let (ns, ctx) = fresh();
    let made = initramfs::unpack(&ns, &ctx, &archive).unwrap();
    assert_eq!(made.init_inputs, 4);
    assert_eq!(made.skipped, 0);
    assert_eq!(made.files, 2);
    assert_eq!(made.directories, 1);
    assert!(ns.resolve(&ctx, None, b"/.ferrix", true).is_err());
    assert!(
        ns.resolve(&ctx, None, b"/.ferrix/init/program", true)
            .is_err()
    );
    assert_eq!(read_file(&ns, &ctx, "/bin/true").unwrap(), b"t");
    assert_eq!(read_file(&ns, &ctx, "/.ferrixish").unwrap(), b"x");

    let entries: Vec<_> = initramfs::init_entries(&archive).collect();
    let names: Vec<&[u8]> = entries.iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        [
            &b".ferrix"[..],
            b".ferrix/init",
            b".ferrix/init/program",
            b".ferrix/init/script"
        ]
    );
    assert!(!entries[0].regular);
    assert!(entries[0].directory && entries[1].directory);
    assert!(!entries[2].directory);
    assert!(entries[2].regular);
    assert_eq!(entries[2].links, 1);
    assert_eq!(entries[2].data, b"\x7fELF");
    assert_eq!(entries[3].data, b"echo hi");
}

#[test]
fn a_truncated_archive_is_an_error_not_a_partial_success() {
    let mut archive = Newc::new();
    archive.entry("file", 0o100_644, 1, 1, (0, 0), b"0123456789");
    let archive = archive.finish();
    let (ns, ctx) = fresh();
    assert!(matches!(
        initramfs::unpack(&ns, &ctx, &archive[..archive.len() - 130]),
        Err(initramfs::UnpackError::Archive(_))
    ));
}

// -- Pipes, poll and statfs --------------------------------------------------

use crate::pipe::{PIPE_BUF, PIPE_CAPACITY, PipeBuffer, ReadOutcome, WriteOutcome};

fn open_pipe(capacity: usize) -> PipeBuffer {
    let mut pipe = PipeBuffer::new(capacity);
    pipe.open_reader();
    pipe.open_writer();
    pipe
}

#[test]
fn a_drained_pipe_is_end_of_file_only_once_no_writer_is_left() {
    let mut pipe = open_pipe(PIPE_CAPACITY);
    let mut buf = [0_u8; 8];
    assert_eq!(pipe.read(&mut buf), ReadOutcome::WouldBlock);
    assert!(!pipe.read_readiness().readable);
    assert_eq!(pipe.write(b"last"), WriteOutcome::Wrote(4));
    pipe.close_writer();
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(4));
    assert_eq!(&buf[..4], b"last");
    assert_eq!(pipe.read(&mut buf), ReadOutcome::EndOfFile);
    let ready = pipe.read_readiness();
    assert!(
        ready.readable && ready.hangup,
        "end of file must wake a poller"
    );
}

fn open_packet_pipe(capacity: usize) -> PipeBuffer {
    let mut pipe = PipeBuffer::packets(capacity);
    pipe.open_reader();
    pipe.open_writer();
    pipe
}

/// A packet pipe (`pipe2(O_DIRECT)`) keeps each write's bounds: a read takes
/// one write at most, however large its buffer, and a stream pipe the same
/// writes as one run of bytes.
#[test]
fn a_packet_pipe_reads_one_write_at_a_time() {
    let mut pipe = open_packet_pipe(PIPE_CAPACITY);
    assert!(pipe.is_packets());
    assert_eq!(pipe.write(b"one"), WriteOutcome::Wrote(3));
    assert_eq!(pipe.write(b"three"), WriteOutcome::Wrote(5));
    assert_eq!(pipe.next_read(), 3);
    let mut buf = [0_u8; 64];
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(3));
    assert_eq!(&buf[..3], b"one");
    assert_eq!(buf[3], 0, "nothing of the next packet is copied");
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(5));
    assert_eq!(&buf[..5], b"three");
    assert_eq!(pipe.read(&mut buf), ReadOutcome::WouldBlock);

    let mut stream = open_pipe(PIPE_CAPACITY);
    assert_eq!(stream.write(b"one"), WriteOutcome::Wrote(3));
    assert_eq!(stream.write(b"three"), WriteOutcome::Wrote(5));
    assert_eq!(stream.read(&mut buf), ReadOutcome::Read(8));
}

/// A read too small for a packet takes what fits and drops the rest of that
/// packet, as Linux's `pipe_read` does; the next read is the next packet.
#[test]
fn a_short_read_of_a_packet_drops_its_rest() {
    let mut pipe = open_packet_pipe(PIPE_CAPACITY);
    assert_eq!(pipe.write(b"abcdef"), WriteOutcome::Wrote(6));
    assert_eq!(pipe.write(b"gh"), WriteOutcome::Wrote(2));
    let mut small = [0_u8; 2];
    assert_eq!(pipe.read(&mut small), ReadOutcome::Read(2));
    assert_eq!(&small, b"ab");
    assert_eq!(pipe.len(), 2, "cdef went with its packet");
    assert_eq!(pipe.read(&mut small), ReadOutcome::Read(2));
    assert_eq!(&small, b"gh");
}

/// A write larger than `PIPE_BUF` goes in as packets of `PIPE_BUF`, one per
/// call, and each needs room for all of itself; bytes put back after a failed
/// copy come back as one packet.
#[test]
fn a_packet_pipe_cuts_large_writes_and_takes_packets_whole() {
    let mut pipe = open_packet_pipe(PIPE_BUF * 2);
    let big = vec![7_u8; PIPE_BUF + 100];
    assert_eq!(pipe.write(&big), WriteOutcome::Wrote(PIPE_BUF));
    assert_eq!(pipe.write(&big[PIPE_BUF..]), WriteOutcome::Wrote(100));
    // PIPE_BUF - 100 bytes of room: a packet of PIPE_BUF waits whole, where a
    // stream would take what fits.
    assert!(
        !pipe.can_write(PIPE_BUF + 100),
        "a whole packet does not fit"
    );
    assert_eq!(pipe.write(&big), WriteOutcome::WouldBlock);
    assert!(pipe.can_write(100));
    let mut buf = vec![0_u8; PIPE_BUF * 2];
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(PIPE_BUF));
    pipe.unread(&buf[..10]);
    assert_eq!(pipe.next_read(), 10);
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(10));
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(100));
    pipe.close_writer();
    assert_eq!(pipe.read(&mut buf), ReadOutcome::EndOfFile);
}

#[test]
fn bytes_a_reader_could_not_take_are_read_again_first() {
    let mut pipe = open_pipe(PIPE_BUF);
    assert_eq!(pipe.write(b"abcdefgh"), WriteOutcome::Wrote(8));
    let mut buf = [0_u8; 6];
    assert_eq!(pipe.read(&mut buf), ReadOutcome::Read(6));
    // A writer fills the room the read made before the reader finds its
    // memory took the first two bytes and faulted on the rest.
    assert_eq!(
        pipe.write(&vec![b'z'; PIPE_BUF - 2]),
        WriteOutcome::Wrote(PIPE_BUF - 2)
    );
    pipe.unread(&buf[2..]);
    pipe.unread(b"");
    // The four come first, ahead of both the two left and the writer's, and
    // the pipe is over its capacity until they are read, never short a byte.
    assert_eq!(pipe.len(), PIPE_BUF + 4);
    assert_eq!(pipe.room(), 0);
    let mut out = vec![0_u8; 2 * PIPE_BUF];
    assert_eq!(pipe.read(&mut out), ReadOutcome::Read(PIPE_BUF + 4));
    assert_eq!(&out[..6], b"cdefgh");
    assert_eq!(pipe.len(), 0);
    assert!(out[6..PIPE_BUF + 4].iter().all(|&b| b == b'z'));
}

#[test]
fn a_write_with_no_reader_is_broken_and_polls_as_an_error() {
    let mut pipe = open_pipe(PIPE_CAPACITY);
    pipe.close_reader();
    assert_eq!(pipe.write(b"x"), WriteOutcome::Broken);
    let ready = pipe.write_readiness();
    assert!(ready.error && ready.writable);
}

#[test]
fn a_small_write_is_never_split_and_a_large_one_takes_what_fits() {
    let mut pipe = open_pipe(2 * PIPE_BUF);
    let big = vec![7_u8; PIPE_BUF + PIPE_BUF / 2];
    assert_eq!(pipe.write(&big), WriteOutcome::Wrote(big.len()));
    let small = vec![1_u8; PIPE_BUF];
    assert_eq!(
        pipe.write(&small),
        WriteOutcome::WouldBlock,
        "a write of PIPE_BUF bytes must not be split"
    );
    assert!(!pipe.write_readiness().writable);
    assert_eq!(pipe.write(&big), WriteOutcome::Wrote(PIPE_BUF / 2));
    assert_eq!(pipe.write(&big), WriteOutcome::WouldBlock);
    let mut out = vec![0_u8; 3 * PIPE_BUF];
    assert_eq!(pipe.read(&mut out), ReadOutcome::Read(2 * PIPE_BUF));
    assert!(pipe.write_readiness().writable);
}

#[test]
fn a_pipe_delivers_bytes_in_order_against_a_model() {
    let mut pipe = open_pipe(PIPE_CAPACITY);
    let mut model = alloc::collections::VecDeque::new();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    // Every write is a run of this, cut rather than made a byte at a time,
    // and every read is checked a slice at a time. Byte loops made this test
    // hours under Miri, the test's own more than the pipe's; now the full
    // length is seconds there too.
    let counting: Vec<u8> = (0..9000 + 256).map(|i| i as u8).collect();
    for _ in 0..4000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = usize::try_from((state >> 8) % 9000).unwrap();
        if state & 1 == 0 {
            let start = usize::from(state as u8);
            let data = &counting[start..start + len];
            match pipe.write(data) {
                WriteOutcome::Wrote(n) => {
                    if data.len() <= PIPE_BUF {
                        assert_eq!(n, data.len(), "a small write was split");
                    }
                    model.extend(&data[..n]);
                }
                WriteOutcome::WouldBlock => {}
                WriteOutcome::Broken => panic!("a reader is open"),
                WriteOutcome::NoMemory => panic!("no job limits this pipe"),
            }
        } else {
            let mut buf = vec![0_u8; len];
            match pipe.read(&mut buf) {
                ReadOutcome::Read(n) => {
                    let (front, back) = model.as_slices();
                    let (first, second) = buf[..n].split_at(front.len().min(n));
                    assert!(
                        first == &front[..first.len()] && second == &back[..second.len()],
                        "bytes came out of order"
                    );
                    drop(model.drain(..n));
                }
                ReadOutcome::WouldBlock => assert!(model.is_empty()),
                ReadOutcome::EndOfFile => panic!("a writer is open"),
            }
        }
        assert_eq!(pipe.len(), model.len());
        assert!(pipe.len() <= PIPE_CAPACITY);
    }
}

#[test]
fn tmpfs_says_what_it_is_and_an_open_file_polls_for_its_access_mode() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", b"x");
    let stat = ns.statfs(&ctx.root);
    assert_eq!(stat.magic, crate::tmpfs::TMPFS_MAGIC);
    assert_eq!(stat.name_max, 255);
    assert_eq!(stat.files, 2, "the root and one file");
    let file = ns.open(&ctx, None, b"/f", &READ, 0).unwrap();
    let ready = file.poll();
    assert!(
        ready.readable && !ready.writable,
        "masked by the access mode"
    );
}

/// A directory whose names change without the VFS being told, as `/proc`'s
/// do when a process starts or exits.
#[derive(Debug, Default)]
struct Volatile {
    names: ferrix_sync::SpinLock<Vec<Vec<u8>>>,
}

impl Volatile {
    fn meta(ino: u64, kind: FileType) -> crate::Metadata {
        crate::Metadata {
            ino,
            kind,
            permissions: 0o555,
            nlink: 1,
            uid: 0,
            gid: 0,
            size: 0,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: Timespec::default(),
            mtime: Timespec::default(),
            ctime: Timespec::default(),
        }
    }
}

#[derive(Debug)]
struct VolatileFile;

impl crate::Inode for VolatileFile {
    fn metadata(&self) -> crate::Metadata {
        Volatile::meta(2, FileType::Regular)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
}

impl crate::Inode for Volatile {
    fn metadata(&self) -> crate::Metadata {
        Volatile::meta(1, FileType::Directory)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn caches_lookups(&self) -> bool {
        false
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn crate::Inode>, Errno> {
        if self.names.lock().iter().any(|held| held == name) {
            Ok(Arc::new(VolatileFile))
        } else {
            Err(Errno::ENOENT)
        }
    }
}

#[derive(Debug)]
struct VolatileFs(Arc<Volatile>);

impl FileSystem for VolatileFs {
    fn root(&self) -> Arc<dyn crate::Inode> {
        Arc::clone(&self.0) as Arc<dyn crate::Inode>
    }

    fn name(&self) -> &'static str {
        "volatile"
    }

    fn device(&self) -> u64 {
        9
    }
}

#[test]
fn a_directory_that_does_not_cache_lookups_is_asked_every_time() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/proc", 0o755).unwrap();
    let dir = Arc::new(Volatile::default());
    let at = ns.resolve(&ctx, None, b"/proc", true).unwrap();
    let _ = ns
        .mount(Arc::new(VolatileFs(Arc::clone(&dir))), &at)
        .unwrap();

    // A miss first, which a caching directory would remember.
    assert_eq!(
        ns.resolve(&ctx, None, b"/proc/42", true).err(),
        Some(Errno::ENOENT),
        "nothing is called 42 yet"
    );
    dir.names.lock().push(b"42".to_vec());
    let found = ns.resolve(&ctx, None, b"/proc/42", true).unwrap();
    assert_eq!(
        ns.path_of(&found, &ctx.root),
        b"/proc/42".to_vec(),
        "an uncached dentry still knows where it is"
    );

    // Then a hit, which must not outlive the name either.
    dir.names.lock().clear();
    assert_eq!(
        ns.resolve(&ctx, None, b"/proc/42", true).err(),
        Some(Errno::ENOENT),
        "a name that went away is gone at once"
    );
}

// -- Streams, detached locations and statfs layouts ------------------------

use core::sync::atomic::AtomicBool;

use crate::StatFs;
use crate::file::Status;
use crate::pipe::PIPEFS_MAGIC;
use crate::statfs::StatfsLayout;

fn stream_metadata() -> crate::Metadata {
    crate::Metadata {
        ino: 7,
        kind: FileType::Fifo,
        permissions: 0o600,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        rdev: 0,
        blocks: 0,
        block_size: 4096,
        atime: Timespec::default(),
        mtime: Timespec::default(),
        ctime: Timespec::default(),
    }
}

/// A stream that remembers whether its last call was told not to wait, and
/// refuses a read that was, as a pipe with nothing in it does.
#[derive(Debug, Default)]
struct Recorder {
    nonblock: AtomicBool,
}

impl crate::Inode for Recorder {
    fn metadata(&self) -> crate::Metadata {
        stream_metadata()
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.nonblock.store(nonblock, Ordering::Relaxed);
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        buf.fill(b'r');
        Ok(buf.len())
    }
    fn write_stream(&self, data: &[u8], nonblock: bool) -> Result<usize, Errno> {
        self.nonblock.store(nonblock, Ordering::Relaxed);
        Ok(data.len())
    }
}

/// A stream that implements only the positioned calls, as the console does.
#[derive(Debug)]
struct Positioned;

impl crate::Inode for Positioned {
    fn metadata(&self) -> crate::Metadata {
        stream_metadata()
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if offset != 0 {
            return Err(Errno::EINVAL);
        }
        buf.fill(b'p');
        Ok(buf.len())
    }
    fn write_at(&self, _offset: u64, data: &[u8], _append: bool) -> Result<(usize, u64), Errno> {
        Ok((data.len(), 0))
    }
}

#[derive(Debug)]
struct Pipes;

impl FileSystem for Pipes {
    fn root(&self) -> Arc<dyn crate::Inode> {
        Arc::new(Positioned)
    }
    fn name(&self) -> &'static str {
        "pipefs"
    }
    fn device(&self) -> u64 {
        99
    }
    fn statfs(&self) -> StatFs {
        StatFs {
            magic: PIPEFS_MAGIC,
            ..StatFs::default()
        }
    }
}

const READ_WRITE: OpenFlags = OpenFlags {
    write: true,
    ..READ
};

#[test]
fn a_stream_is_told_whether_its_open_file_may_wait() {
    let recorder = Arc::new(Recorder::default());
    let at = Location::detached(
        Arc::new(Pipes),
        recorder.clone(),
        b"pipe:[7]",
        Arc::new(SpinParker),
    )
    .unwrap();
    let file = OpenFile::new(at, &READ_WRITE).unwrap();
    let mut buf = [0_u8; 4];
    assert_eq!(file.read(&mut buf), Ok(4));
    assert!(!recorder.nonblock.load(Ordering::Relaxed));

    file.set_status(Status {
        append: false,
        nonblock: true,
    });
    assert_eq!(
        file.read(&mut buf),
        Err(Errno::EAGAIN),
        "F_SETFL's O_NONBLOCK reaches the next read"
    );
    assert_eq!(file.write(b"xy"), Ok(2));
    assert!(recorder.nonblock.load(Ordering::Relaxed));
    assert_eq!(file.read_at(0, &mut buf), Err(Errno::ESPIPE));
    assert_eq!(file.seek(0, Whence::Current), Err(Errno::ESPIPE));

    // A stream that knows nothing of the flag keeps working through the
    // defaults, which is what leaves the console unchanged.
    let at = Location::detached(
        Arc::new(Pipes),
        Arc::new(Positioned),
        b"console",
        Arc::new(SpinParker),
    )
    .unwrap();
    let console = OpenFile::new(at, &READ_WRITE).unwrap();
    console.set_status(Status {
        append: false,
        nonblock: true,
    });
    assert_eq!(console.read(&mut buf), Ok(4));
    assert_eq!(&buf, b"pppp");
    assert_eq!(console.write(b"out"), Ok(3));
}

/// A stream whose position never moves, as `/dev/zero`'s does: zeros to
/// every read, positioned or not, and every write taken.
#[derive(Debug)]
struct Zeros;

impl crate::Inode for Zeros {
    fn metadata(&self) -> crate::Metadata {
        crate::Metadata {
            kind: FileType::CharDevice,
            ..stream_metadata()
        }
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn ignores_position(&self) -> bool {
        true
    }
    fn read_stream(&self, buf: &mut [u8], _nonblock: bool) -> Result<usize, Errno> {
        buf.fill(0);
        Ok(buf.len())
    }
    fn write_stream(&self, data: &[u8], _nonblock: bool) -> Result<usize, Errno> {
        Ok(data.len())
    }
}

#[test]
fn a_stream_that_ignores_its_position_seeks_to_zero_and_takes_offsets() {
    let at = Location::detached(
        Arc::new(Pipes),
        Arc::new(Zeros),
        b"zero",
        Arc::new(SpinParker),
    )
    .unwrap();
    let zero = OpenFile::new(at, &READ_WRITE).unwrap();
    assert!(zero.is_stream() && zero.takes_offsets());
    for (offset, whence) in [
        (100, Whence::Set),
        (-10, Whence::Set),
        (-5, Whence::Current),
        (10, Whence::End),
        (3, Whence::Data),
        (3, Whence::Hole),
    ] {
        assert_eq!(zero.seek(offset, whence), Ok(0), "{offset} {whence:?}");
    }
    let mut buf = [0xA5_u8; 4];
    assert_eq!(zero.read(&mut buf), Ok(4));
    assert_eq!(zero.seek(0, Whence::Current), Ok(0), "a read moves nothing");
    buf.fill(0xA5);
    assert_eq!(zero.read_at(1000, &mut buf), Ok(4));
    assert_eq!(buf, [0; 4]);
    assert_eq!(zero.write_at(1000, b"xy"), Ok(2));

    // A pipe still refuses all three.
    let at = Location::detached(
        Arc::new(Pipes),
        Arc::new(Recorder::default()),
        b"pipe:[7]",
        Arc::new(SpinParker),
    )
    .unwrap();
    let pipe = OpenFile::new(at, &READ_WRITE).unwrap();
    assert!(!pipe.takes_offsets());
    assert_eq!(pipe.seek(0, Whence::Set), Err(Errno::ESPIPE));
    assert_eq!(pipe.read_at(0, &mut buf), Err(Errno::ESPIPE));
    assert_eq!(pipe.write_at(0, b"x"), Err(Errno::ESPIPE));
}

/// A stream whose `lseek` does nothing, as an eventfd's does.
#[derive(Debug)]
struct Counter;

impl crate::Inode for Counter {
    fn metadata(&self) -> crate::Metadata {
        stream_metadata()
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn seek_is_noop(&self) -> bool {
        true
    }
    fn read_stream(&self, buf: &mut [u8], _nonblock: bool) -> Result<usize, Errno> {
        buf.fill(1);
        Ok(buf.len())
    }
}

#[test]
fn a_stream_whose_seek_does_nothing_seeks_to_zero_and_refuses_offsets() {
    let at = Location::detached(
        Arc::new(Pipes),
        Arc::new(Counter),
        b"anon_inode:[eventfd]",
        Arc::new(SpinParker),
    )
    .unwrap();
    let counter = OpenFile::new(at, &READ_WRITE).unwrap();
    assert!(counter.is_stream() && !counter.takes_offsets());
    for (offset, whence) in [
        (100, Whence::Set),
        (-10, Whence::Set),
        (5, Whence::Current),
        (10, Whence::End),
        (0, Whence::Data),
        (0, Whence::Hole),
    ] {
        assert_eq!(counter.seek(offset, whence), Ok(0), "{offset} {whence:?}");
    }
    let mut buf = [0_u8; 8];
    assert_eq!(counter.read(&mut buf), Ok(8));
    assert_eq!(
        counter.seek(0, Whence::Current),
        Ok(0),
        "a read moves nothing"
    );
    assert_eq!(counter.read_at(0, &mut buf), Err(Errno::ESPIPE));
    assert_eq!(counter.write_at(0, b"x"), Err(Errno::ESPIPE));

    // And a stream that ignores its position has a seek that does nothing,
    // without saying so twice.
    assert!(crate::Inode::seek_is_noop(&Zeros));
    assert!(!crate::Inode::seek_is_noop(&Recorder::default()));
}

/// A stream that reads the same at any offset but cannot be sought, as a
/// DRM card is on Linux: `pread64` is a `read`, `lseek` is `ESPIPE`.
#[derive(Debug)]
struct Events;

impl crate::Inode for Events {
    fn metadata(&self) -> crate::Metadata {
        crate::Metadata {
            kind: FileType::CharDevice,
            ..stream_metadata()
        }
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn ignores_position(&self) -> bool {
        true
    }
    fn seek_is_noop(&self) -> bool {
        false
    }
    fn read_stream(&self, _buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        // Opened O_NONBLOCK: a read told it may wait is the test's mistake.
        if !nonblock {
            return Err(Errno::EDEADLK);
        }
        Err(Errno::EAGAIN)
    }
}

#[test]
fn a_stream_that_takes_offsets_may_still_refuse_lseek() {
    let at = Location::detached(
        Arc::new(Pipes),
        Arc::new(Events),
        b"card0",
        Arc::new(SpinParker),
    )
    .unwrap();
    let flags = OpenFlags {
        nonblock: true,
        ..READ_WRITE
    };
    let card = OpenFile::new(at, &flags).unwrap();
    assert!(card.takes_offsets());
    assert_eq!(card.seek(0, Whence::Set), Err(Errno::ESPIPE));
    assert_eq!(card.seek(0, Whence::Current), Err(Errno::ESPIPE));
    let mut buf = [0_u8; 64];
    assert_eq!(card.read_at(1000, &mut buf), Err(Errno::EAGAIN));
    assert_eq!(card.read(&mut buf), Err(Errno::EAGAIN));
    // Written to as a card is, which has no write: EINVAL, not ESPIPE.
    assert_eq!(card.write_at(0, b"x"), Err(Errno::EINVAL));
}

/// A stream that fills reads, as a pipe does, holding `left` bytes: a read
/// that would wait counts itself and is refused as a blocking pipe's would
/// hang.
#[derive(Debug)]
struct Tap {
    left: AtomicUsize,
    waits: AtomicUsize,
}

impl crate::Inode for Tap {
    fn metadata(&self) -> crate::Metadata {
        stream_metadata()
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn fills_reads(&self) -> bool {
        true
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        let left = self.left.load(Ordering::Relaxed);
        if left == 0 {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _ = self.waits.fetch_add(1, Ordering::Relaxed);
            return Err(Errno::EDEADLK);
        }
        let count = left.min(buf.len());
        buf[..count].fill(b't');
        self.left.store(left - count, Ordering::Relaxed);
        Ok(count)
    }
}

#[test]
fn a_read_goes_on_only_from_a_stream_that_fills_and_never_waits() {
    let open = |inode: Arc<dyn crate::Inode>, flags: &OpenFlags| {
        let at = Location::detached(Arc::new(Pipes), inode, b"s", Arc::new(SpinParker)).unwrap();
        OpenFile::new(at, flags).unwrap()
    };
    let tap = Arc::new(Tap {
        left: AtomicUsize::new(6),
        waits: AtomicUsize::new(0),
    });
    let file = open(Arc::clone(&tap) as Arc<dyn crate::Inode>, &READ);
    let mut buf = [0_u8; 4];
    assert_eq!(file.read(&mut buf), Ok(4));
    assert_eq!(file.read_more(&mut buf), Ok(2), "what is there is taken");
    assert_eq!(file.read_more(&mut buf), Ok(0), "an empty stream ends it");
    assert_eq!(
        tap.waits.load(Ordering::Relaxed),
        0,
        "and is never waited on"
    );

    // A stream of records is not read on, and not even asked: a second
    // read of an eventfd would take its next count.
    let counter = open(Arc::new(Counter), &READ);
    assert_eq!(counter.read_more(&mut buf), Ok(0));
    assert_eq!(open(Arc::new(Zeros), &READ).read_more(&mut buf), Ok(0));

    // And a file not open for reading is refused as a read is.
    let write_only = OpenFlags {
        read: false,
        ..READ_WRITE
    };
    assert_eq!(
        open(
            Arc::new(Tap {
                left: AtomicUsize::new(1),
                waits: AtomicUsize::new(0),
            }),
            &write_only
        )
        .read_more(&mut buf),
        Err(Errno::EBADF)
    );
}

#[test]
fn a_detached_location_opens_and_names_itself_without_a_tree() {
    let (ns, ctx) = fresh();
    let at = Location::detached(
        Arc::new(Pipes),
        Arc::new(Recorder::default()),
        b"pipe:[7]",
        Arc::new(SpinParker),
    )
    .unwrap();
    assert!(at.is_detached() && !ctx.root.is_detached());
    assert_eq!(ns.path_of(&at, &ctx.root), b"pipe:[7]");
    assert_eq!(
        ns.stat(&at).unwrap().dev,
        99,
        "the device is its filesystem's"
    );
    assert_eq!(ns.statfs(&at).magic, PIPEFS_MAGIC);
    assert!(at.parent().same(&at), "`..` from nowhere stays there");
    assert_eq!(ns.unmount(&at), Err(Errno::EINVAL), "it is not mounted");
    let file = OpenFile::new(at, &READ).unwrap();
    assert_eq!(file.kind(), FileType::Fifo);
    assert_eq!(ns.path_of(file.location(), &ctx.root), b"pipe:[7]");
}

#[test]
fn with_io_sends_reads_elsewhere_and_keeps_what_stat_reports() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", b"abc");
    let file = ns.open(&ctx, None, b"/f", &READ, 0).unwrap();
    let mut buf = [0_u8; 3];
    assert_eq!(file.read(&mut buf), Ok(3));

    let recorder = Arc::new(Recorder::default());
    let swapped = file.with_io(recorder).unwrap();
    assert!(Arc::ptr_eq(swapped.inode(), file.inode()));
    assert!(swapped.location().same(file.location()));
    assert!(swapped.readable() && !swapped.writable());
    assert_eq!(swapped.offset(), 0, "a new open starts at the beginning");
    assert_eq!(swapped.read(&mut buf), Ok(3));
    assert_eq!(&buf, b"rrr", "the read went to the new object");
    assert_eq!(
        swapped.write(b"no"),
        Err(Errno::EBADF),
        "the access mode came too"
    );
}

#[test]
fn a_blocked_pipe_end_waits_for_exactly_what_lets_it_proceed() {
    let mut pipe = open_pipe(2 * PIPE_BUF);
    assert!(!pipe.can_read());
    assert_eq!(
        pipe.write(&vec![0_u8; 2 * PIPE_BUF]),
        WriteOutcome::Wrote(2 * PIPE_BUF)
    );
    assert!(pipe.can_read());
    assert!(!pipe.can_write(1) && !pipe.can_write(PIPE_BUF + 1));
    assert!(pipe.can_write(0), "an empty write never waits");

    let mut one = [0_u8; 1];
    assert_eq!(pipe.read(&mut one), ReadOutcome::Read(1));
    assert!(pipe.can_write(1) && !pipe.can_write(2));
    assert_eq!(
        pipe.write(&[1, 2]),
        WriteOutcome::WouldBlock,
        "can_write agrees with write for a small write"
    );
    assert!(pipe.can_write(PIPE_BUF + 1), "a large write takes any room");
    pipe.close_reader();
    assert!(
        pipe.can_write(PIPE_BUF),
        "and wakes to find the pipe broken"
    );

    let mut empty = open_pipe(PIPE_CAPACITY);
    empty.close_writer();
    assert!(empty.can_read(), "and a reader wakes to end of file");
}

fn le32(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()))
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[test]
fn statfs_packs_each_layout_at_the_headers_offsets() {
    let stat = StatFs {
        magic: crate::tmpfs::TMPFS_MAGIC,
        block_size: 4096,
        blocks: 1000,
        blocks_free: 600,
        blocks_available: 500,
        files: 12,
        files_free: 3,
        name_max: 255,
    };
    let wide = StatfsLayout::Wide.encode(&stat).unwrap();
    assert_eq!(wide.len(), StatfsLayout::Wide.size());
    let fields = [0, 8, 16, 24, 32, 40, 48, 64, 72, 80].map(|at| le64(&wide, at));
    assert_eq!(
        fields,
        [0x0102_1994, 4096, 1000, 600, 500, 12, 3, 255, 4096, 0x20]
    );
    assert!(wide[56..64].iter().chain(&wide[88..]).all(|&b| b == 0));

    let narrow = StatfsLayout::Narrow.encode(&stat).unwrap();
    assert_eq!(narrow.len(), 64);
    let fields = [0, 4, 8, 12, 16, 20, 24, 36, 40, 44].map(|at| le32(&narrow, at));
    assert_eq!(
        fields,
        [0x0102_1994, 4096, 1000, 600, 500, 12, 3, 255, 4096, 0x20]
    );

    let packed = StatfsLayout::Packed64.encode(&stat).unwrap();
    assert_eq!(packed.len(), 84);
    assert_eq!([le32(&packed, 0), le32(&packed, 4)], [0x0102_1994, 4096]);
    let counts = [8, 16, 24, 32, 40].map(|at| le64(&packed, at));
    assert_eq!(counts, [1000, 600, 500, 12, 3]);
    let tail = [56, 60, 64].map(|at| le32(&packed, at));
    assert_eq!(tail, [255, 4096, 0x20]);
    assert!(packed[48..56].iter().chain(&packed[68..]).all(|&b| b == 0));

    let big = StatFs {
        blocks: 1 << 32,
        files: u64::MAX,
        ..stat
    };
    assert_eq!(StatfsLayout::Narrow.encode(&big), Err(Errno::EOVERFLOW));
    assert_eq!(
        le64(&StatfsLayout::Packed64.encode(&big).unwrap(), 8),
        1 << 32
    );
    let unlimited = StatFs {
        files: u64::MAX,
        ..stat
    };
    let narrow = StatfsLayout::Narrow.encode(&unlimited).unwrap();
    assert_eq!(le32(&narrow, 20), 0xFFFF_FFFF, "-1 passes at either width");
    assert_eq!(StatfsLayout::native(8), StatfsLayout::Wide);
    assert_eq!(StatfsLayout::native(4), StatfsLayout::Narrow);
}

#[test]
fn growing_a_file_never_shrinks_it_and_needs_it_open_for_writing() {
    let (ns, ctx) = fresh();
    write_file(&ns, &ctx, "/f", b"0123456789");
    let file = ns.open(&ctx, None, b"/f", &RW_CREATE, 0o644).unwrap();
    file.grow_to(4).unwrap();
    assert_eq!(
        file.inode().metadata().size,
        10,
        "growing to less is no change"
    );
    file.grow_to(5000).unwrap();
    let grown = read_file(&ns, &ctx, "/f").unwrap();
    assert_eq!(grown.len(), 5000);
    assert_eq!(&grown[..12], b"0123456789\0\0", "what it uncovers is zeros");
    let reader = ns.open(&ctx, None, b"/f", &READ, 0).unwrap();
    assert_eq!(reader.grow_to(9000), Err(Errno::EINVAL));
    let dir = ns.open(&ctx, None, b"/", &READ, 0).unwrap();
    assert_eq!(dir.grow_to(1), Err(Errno::EINVAL));
}

// -- Races, made to happen on one thread -------------------------------------

/// When a [`Meddling`] directory runs its hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Moment {
    /// After the filesystem has answered a lookup, before the VFS records it.
    Lookup,
    /// After the VFS decided a name is missing, before the filesystem creates it.
    Create,
}

/// Something another thread could have done, run at a chosen moment.
type Hook = alloc::boxed::Box<dyn FnOnce() + Send>;

/// At most one armed hook, shared by every inode of a meddling filesystem.
#[derive(Default)]
struct Hooks {
    armed: ferrix_sync::SpinLock<Option<(Moment, Vec<u8>, Hook)>>,
}

impl core::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

impl Hooks {
    fn arm(&self, moment: Moment, name: &[u8], hook: impl FnOnce() + Send + 'static) {
        *self.armed.lock() = Some((moment, name.to_vec(), alloc::boxed::Box::new(hook)));
    }

    /// Run the hook if it is armed for this moment and name, once, with the
    /// lock released so that the hook may use the filesystem.
    fn fire(&self, moment: Moment, name: &[u8]) {
        let hook = {
            let mut armed = self.armed.lock();
            match armed.take() {
                Some((at, armed_name, hook)) if at == moment && armed_name == name => Some(hook),
                other => {
                    *armed = other;
                    None
                }
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// A tmpfs inode that lets a test interleave another operation with a lookup
/// or a create, which is how a second thread gets in on a real machine.
#[derive(Debug)]
struct Meddling {
    inner: Arc<dyn crate::Inode>,
    hooks: Arc<Hooks>,
}

impl Meddling {
    fn wrap(&self, inner: Arc<dyn crate::Inode>) -> Arc<dyn crate::Inode> {
        Arc::new(Meddling {
            inner,
            hooks: Arc::clone(&self.hooks),
        })
    }
}

impl crate::Inode for Meddling {
    fn metadata(&self) -> crate::Metadata {
        self.inner.metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64), Errno> {
        self.inner.write_at(offset, data, append)
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn crate::Inode>, Errno> {
        let found = self.inner.lookup(name);
        self.hooks.fire(Moment::Lookup, name);
        found.map(|inner| self.wrap(inner))
    }

    fn create(
        &self,
        name: &[u8],
        node: crate::NewNode<'_>,
        permissions: u32,
    ) -> Result<Arc<dyn crate::Inode>, Errno> {
        self.hooks.fire(Moment::Create, name);
        self.inner
            .create(name, node, permissions)
            .map(|inner| self.wrap(inner))
    }

    fn unlink(&self, name: &[u8]) -> Result<(), Errno> {
        self.inner.unlink(name)
    }

    fn rmdir(&self, name: &[u8]) -> Result<(), Errno> {
        self.inner.rmdir(name)
    }

    fn rename(
        &self,
        old: &[u8],
        new_parent: &Arc<dyn crate::Inode>,
        new: &[u8],
        replace: bool,
    ) -> Result<(), Errno> {
        let new_parent = Arc::clone(new_parent)
            .into_any()
            .downcast::<Meddling>()
            .map_err(|_| Errno::EXDEV)?;
        self.inner.rename(old, &new_parent.inner, new, replace)
    }

    fn read_dir(
        &self,
        cursor: u64,
        emit: &mut dyn FnMut(crate::DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        self.inner.read_dir(cursor, emit)
    }
}

#[derive(Debug)]
struct MeddlingFs(Arc<Meddling>);

impl FileSystem for MeddlingFs {
    fn root(&self) -> Arc<dyn crate::Inode> {
        Arc::clone(&self.0) as Arc<dyn crate::Inode>
    }

    fn name(&self) -> &'static str {
        "meddling"
    }

    fn device(&self) -> u64 {
        1
    }
}

/// A namespace over a meddling tmpfs keeping `cache` unused dentries, and the
/// hooks that meddle with it.
fn meddling(cache: usize) -> (Arc<Namespace>, Arc<Hooks>) {
    let hooks = Arc::new(Hooks::default());
    let root = Arc::new(Meddling {
        inner: tmpfs(1).root(),
        hooks: Arc::clone(&hooks),
    });
    let ns = Namespace::with_cache(Arc::new(MeddlingFs(root)), cache, Arc::new(SpinParker));
    (Arc::new(ns), hooks)
}

const DIRECTORY: OpenFlags = OpenFlags {
    directory: true,
    ..READ
};

#[test]
fn a_lookup_that_loses_a_race_does_not_let_a_directory_move_inside_itself() {
    // No cache, so that every walk to /A/X asks the filesystem again.
    let (ns, hooks) = meddling(0);
    let ctx = ns.context();
    ns.mkdir(&ctx, None, b"/A", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/A/X", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/B", 0o755).unwrap();

    // While /A/X is being looked up, a name in /A changes, as it would if
    // another thread created a file there.
    let other = Arc::clone(&ns);
    hooks.arm(Moment::Lookup, b"X", move || {
        let ctx = other.context();
        other
            .mknod(&ctx, None, b"/A/junk", crate::NewNode::Fifo, 0o644)
            .unwrap();
    });
    let x = ns.open(&ctx, None, b"/A/X", &DIRECTORY, 0).unwrap();
    let again = ns.resolve(&ctx, None, b"/A/X", true).unwrap();
    assert!(
        Arc::ptr_eq(&x.location().dentry, &again.dentry),
        "one directory has two dentries"
    );
    drop(again);

    ns.rename(&ctx, (None, b"/A/X"), (None, b"/B/X"), RenameMode::Replace)
        .unwrap();
    assert_eq!(ns.path_of(x.location(), &ctx.root), b"/B/X");
    assert_eq!(
        ns.rename(
            &ctx,
            (None, b"/B"),
            (Some(x.location()), b"sub"),
            RenameMode::Replace
        ),
        Err(Errno::EINVAL),
        "/B was moved into its own child"
    );
    assert_eq!(kind(&ns, &ctx, "/B/X", true), Ok(FileType::Directory));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "threads under Miri take minutes, and the test above is deterministic"
)]
fn a_directory_has_one_dentry_while_its_parent_churns() {
    extern crate std;

    let ns = Arc::new(Namespace::with_cache(tmpfs(1), 0, Arc::new(SpinParker)));
    let ctx = ns.context();
    ns.mkdir(&ctx, None, b"/A", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/A/X", 0o755).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let ns = Arc::clone(&ns);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let ctx = ns.context();
            while !stop.load(Ordering::Relaxed) {
                let _ = ns.mknod(&ctx, None, b"/A/junk", crate::NewNode::Fifo, 0o644);
                let _ = ns.unlink(&ctx, None, b"/A/junk");
            }
        })
    };
    let started = std::time::Instant::now();
    let mut split = 0_u32;
    while started.elapsed() < core::time::Duration::from_millis(300) {
        let x = ns.open(&ctx, None, b"/A/X", &DIRECTORY, 0).unwrap();
        let again = ns.resolve(&ctx, None, b"/A/X", true).unwrap();
        if !Arc::ptr_eq(&x.location().dentry, &again.dentry) {
            split += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    churn.join().unwrap();
    assert_eq!(split, 0, "a walk handed out a second dentry for /A/X");
}

#[test]
fn tmpfs_refuses_to_move_a_directory_into_itself_whatever_the_vfs_checked() {
    let fs = tmpfs(1);
    let root = fs.root();
    let a = root.create(b"a", crate::NewNode::Directory, 0o755).unwrap();
    let b = a.create(b"b", crate::NewNode::Directory, 0o755).unwrap();
    assert_eq!(root.rename(b"a", &b, b"inside", true), Err(Errno::EINVAL));
    assert_eq!(root.rename(b"a", &a, b"itself", true), Err(Errno::EINVAL));
    assert!(root.lookup(b"a").is_ok(), "a refused move moved nothing");

    // A move that is not into itself still works, and afterwards the moved
    // directory's new ancestors are the ones checked.
    let c = root.create(b"c", crate::NewNode::Directory, 0o755).unwrap();
    root.rename(b"c", &b, b"c", true).unwrap();
    assert_eq!(b.rename(b"c", &c, b"x", true), Err(Errno::EINVAL));
    assert_eq!(root.rename(b"a", &c, b"a", true), Err(Errno::EINVAL));
}

#[test]
fn open_create_without_excl_opens_a_file_created_under_it() {
    let (ns, hooks) = meddling(crate::DEFAULT_CACHE);
    let ctx = ns.context();
    let append = OpenFlags {
        append: true,
        ..RW_CREATE
    };

    // Two `echo >> log` on a new file: the other one creates it between this
    // one's walk finding nothing and its create.
    let other = Arc::clone(&ns);
    hooks.arm(Moment::Create, b"log", move || {
        let ctx = other.context();
        let file = other.open(&ctx, None, b"/log", &append, 0o644).unwrap();
        assert_eq!(file.write(b"first\n"), Ok(6));
    });
    let file = ns
        .open(&ctx, None, b"/log", &append, 0o644)
        .expect("O_CREAT without O_EXCL is not EEXIST");
    assert_eq!(file.write(b"second\n"), Ok(7));
    assert_eq!(read_file(&ns, &ctx, "/log").unwrap(), b"first\nsecond\n");

    // With O_EXCL the loser is told, as it must be.
    let other = Arc::clone(&ns);
    hooks.arm(Moment::Create, b"lock", move || {
        let ctx = other.context();
        let _ = other.open(&ctx, None, b"/lock", &RW_CREATE, 0o644).unwrap();
    });
    let exclusive = OpenFlags {
        exclusive: true,
        ..RW_CREATE
    };
    assert_eq!(
        ns.open(&ctx, None, b"/lock", &exclusive, 0o644)
            .unwrap_err(),
        Errno::EEXIST
    );
}

// -- `..` in a listing -------------------------------------------------------

/// The inode number a listing of `path` gives `..`.
fn listed_dotdot(ns: &Namespace, ctx: &Context, path: &str) -> u64 {
    let dir = ns.open(ctx, None, path.as_bytes(), &DIRECTORY, 0).unwrap();
    let mut dotdot = None;
    dir.read_dir(&mut |entry| {
        if entry.name == b".." {
            dotdot = Some(entry.ino);
        }
        true
    })
    .unwrap();
    dotdot.expect("a listing has ..")
}

fn ino(ns: &Namespace, ctx: &Context, path: &str) -> u64 {
    let at = ns.resolve(ctx, None, path.as_bytes(), true).unwrap();
    ns.stat(&at).unwrap().metadata.ino
}

#[test]
fn dotdot_in_a_listing_is_the_parent_even_across_a_mount() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/d/e", 0o755).unwrap();
    assert_eq!(listed_dotdot(&ns, &ctx, "/d/e"), ino(&ns, &ctx, "/d"));
    assert_eq!(listed_dotdot(&ns, &ctx, "/d"), ino(&ns, &ctx, "/"));
    assert_eq!(
        listed_dotdot(&ns, &ctx, "/"),
        ino(&ns, &ctx, "/"),
        "the root is its own parent"
    );

    // `..` of a mount's root is the parent of the directory it covers, in
    // the filesystem underneath, as a walk of `/d/mnt/..` finds.
    ns.mkdir(&ctx, None, b"/d/mnt", 0o755).unwrap();
    let covered = ns.resolve(&ctx, None, b"/d/mnt", true).unwrap();
    let _ = ns.mount(tmpfs(2), &covered).unwrap();
    ns.mkdir(&ctx, None, b"/d/mnt/sub", 0o755).unwrap();
    assert_eq!(listed_dotdot(&ns, &ctx, "/d/mnt"), ino(&ns, &ctx, "/d"));
    assert_eq!(
        listed_dotdot(&ns, &ctx, "/d/mnt"),
        ino(&ns, &ctx, "/d/mnt/.."),
        "a listing and a walk disagree about .."
    );
    assert_eq!(
        listed_dotdot(&ns, &ctx, "/d/mnt/sub"),
        ino(&ns, &ctx, "/d/mnt")
    );
}

#[test]
fn a_directory_replaced_by_rename_is_freed_even_after_misses_inside_it() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/new", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/old", 0o755).unwrap();
    let replaced = {
        let at = ns.resolve(&ctx, None, b"/old", true).unwrap();
        Arc::downgrade(&at.inode().unwrap())
    };
    for name in ["/old/missing", "/old/also-missing"] {
        let _ = ns.resolve(&ctx, None, name.as_bytes(), true);
    }
    ns.rename(&ctx, (None, b"/new"), (None, b"/old"), RenameMode::Replace)
        .unwrap();
    assert!(
        replaced.upgrade().is_none(),
        "cached misses inside a replaced directory kept it alive"
    );
}

// -- The description's locks across I/O -------------------------------------

use core::sync::atomic::AtomicUsize;

/// A regular file that notes whether any call into it found its open file's
/// offset lock held, which a positioned read or write must and `pread` must
/// not, and whether any found a spin lock of the description held, which
/// nothing may, since a btrfs read sleeps on the disk in there.
struct Watched {
    file: ferrix_sync::SpinLock<alloc::sync::Weak<OpenFile>>,
    bytes: ferrix_sync::SpinLock<Vec<u8>>,
    /// The most one read returns; zero for no limit.
    short: usize,
    calls: AtomicUsize,
    held: AtomicBool,
    /// Whether every call so far found the offset lock held.
    always_held: AtomicBool,
    /// Whether any call found the status lock held.
    status_held: AtomicBool,
}

impl core::fmt::Debug for Watched {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Watched").finish_non_exhaustive()
    }
}

impl Watched {
    fn new(bytes: &[u8], short: usize) -> Arc<Watched> {
        Arc::new(Watched {
            file: ferrix_sync::SpinLock::new(alloc::sync::Weak::new()),
            bytes: ferrix_sync::SpinLock::new(bytes.to_vec()),
            short,
            calls: AtomicUsize::new(0),
            held: AtomicBool::new(false),
            always_held: AtomicBool::new(true),
            status_held: AtomicBool::new(false),
        })
    }

    /// The open file whose lock the calls look at.
    fn attach(&self, file: &Arc<OpenFile>) {
        *self.file.lock() = Arc::downgrade(file);
    }

    fn called(&self) {
        let _ = self.calls.fetch_add(1, Ordering::Relaxed);
        let Some(file) = self.file.lock().upgrade() else {
            return;
        };
        if file.offset_lock_held() {
            self.held.store(true, Ordering::Relaxed);
        } else {
            self.always_held.store(false, Ordering::Relaxed);
        }
        if file.status_lock_held() {
            self.status_held.store(true, Ordering::Relaxed);
        }
    }
}

impl crate::Inode for Watched {
    fn metadata(&self) -> crate::Metadata {
        crate::Metadata {
            kind: FileType::Regular,
            size: self.bytes.lock().len() as u64,
            ..stream_metadata()
        }
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.called();
        let bytes = self.bytes.lock();
        let start = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        let Some(rest) = bytes.get(start..) else {
            return Ok(0);
        };
        let limit = if self.short == 0 {
            usize::MAX
        } else {
            self.short
        };
        let take = rest.len().min(buf.len()).min(limit);
        buf[..take].copy_from_slice(&rest[..take]);
        Ok(take)
    }
    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64), Errno> {
        self.called();
        let mut bytes = self.bytes.lock();
        let start = if append {
            bytes.len()
        } else {
            usize::try_from(offset).map_err(|_| Errno::EINVAL)?
        };
        let end = start + data.len();
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[start..end].copy_from_slice(data);
        Ok((data.len(), end as u64))
    }
    fn read_dir(
        &self,
        cursor: u64,
        emit: &mut dyn FnMut(crate::DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        self.called();
        if cursor == crate::FIRST_CURSOR {
            let _ = emit(crate::DirEntry {
                ino: 9,
                kind: FileType::Regular,
                name: b"x",
                next: cursor + 1,
            });
        }
        Ok(())
    }
}

const APPEND_CREATE: OpenFlags = OpenFlags {
    append: true,
    ..RW_CREATE
};

#[test]
fn the_offset_lock_is_held_across_a_positioned_call_and_no_spin_lock_is() {
    let (ns, ctx) = fresh();
    let plain = ns.open(&ctx, None, b"/f", &RW_CREATE, 0o644).unwrap();
    let appending = ns.open(&ctx, None, b"/f", &APPEND_CREATE, 0o644).unwrap();
    ns.mkdir(&ctx, None, b"/d", 0o755).unwrap();
    let dir = ns.open(&ctx, None, b"/d", &DIRECTORY, 0).unwrap();

    let watched = Watched::new(b"", 0);
    for opened in [&plain, &appending, &dir] {
        let file = opened
            .with_io(Arc::clone(&watched) as Arc<dyn crate::Inode>)
            .unwrap();
        watched.attach(&file);
        if file.kind() == FileType::Directory {
            let mut listed = 0;
            file.read_dir(&mut |_| {
                listed += 1;
                true
            })
            .unwrap();
            assert_eq!(listed, 3, "., .. and the one entry");
        } else {
            assert_eq!(file.write(b"hello"), Ok(5));
            assert_eq!(file.seek(0, Whence::Set), Ok(0));
            let mut buf = [0_u8; 5];
            assert_eq!(file.read(&mut buf), Ok(5));
        }
    }
    assert_eq!(watched.calls.load(Ordering::Relaxed), 5);
    assert!(
        watched.always_held.load(Ordering::Relaxed),
        "a positioned read, write or listing must hold the offset across the call"
    );
    assert!(
        !watched.status_held.load(Ordering::Relaxed),
        "a file was called into with its description's status spin lock held"
    );
}

#[test]
fn pread_and_pwrite_leave_the_offset_lock_alone() {
    let (ns, ctx) = fresh();
    let opened = ns.open(&ctx, None, b"/f", &RW_CREATE, 0o644).unwrap();
    let watched = Watched::new(b"0123456789", 0);
    let file = opened
        .with_io(Arc::clone(&watched) as Arc<dyn crate::Inode>)
        .unwrap();
    watched.attach(&file);
    let mut buf = [0_u8; 4];
    assert_eq!(file.read_at(2, &mut buf), Ok(4));
    assert_eq!(&buf, b"2345");
    assert_eq!(file.write_at(0, b"ab"), Ok(2));
    assert_eq!(watched.calls.load(Ordering::Relaxed), 2);
    assert!(
        !watched.held.load(Ordering::Relaxed),
        "pread and pwrite position themselves and take no lock"
    );
    assert_eq!(file.offset(), 0, "and leave the position alone");
}

#[test]
fn reads_racing_on_one_description_get_consecutive_bytes() {
    const BYTES: usize = 128;
    const READERS: usize = 4;
    let (ns, ctx) = fresh();
    let file = ns.open(&ctx, None, b"/f", &RW_CREATE, 0o644).unwrap();
    let all: Vec<u8> = (0..BYTES as u8).collect();
    assert_eq!(file.write(&all), Ok(BYTES));
    assert_eq!(file.seek(0, Whence::Set), Ok(0));
    let start = Arc::new(std::sync::Barrier::new(READERS));
    let handles: Vec<_> = (0..READERS)
        .map(|_| {
            let file = Arc::clone(&file);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                let _ = start.wait();
                let mut got = Vec::new();
                loop {
                    let mut byte = [0_u8; 1];
                    match file.read(&mut byte) {
                        Ok(1) => got.push(byte[0]),
                        Ok(_) => break,
                        Err(e) => panic!("read failed: {e:?}"),
                    }
                }
                got
            })
        })
        .collect();
    let mut seen: Vec<u8> = handles
        .into_iter()
        .flat_map(|handle| handle.join().expect("a reader panicked"))
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen, all,
        "every byte read exactly once: the offset was held across each read"
    );
    assert_eq!(file.offset(), BYTES as u64);
}

#[test]
fn sequential_reads_advance_the_offset_by_what_each_got() {
    let (ns, ctx) = fresh();
    let file = ns.open(&ctx, None, b"/f", &RW_CREATE, 0o644).unwrap();
    assert_eq!(file.write(b"abcdefghij"), Ok(10));
    assert_eq!(file.seek(0, Whence::Set), Ok(0));
    let mut buf = [0_u8; 4];
    assert_eq!(file.read(&mut buf), Ok(4));
    assert_eq!(&buf, b"abcd");
    assert_eq!(file.offset(), 4);
    assert_eq!(file.read(&mut buf), Ok(4));
    assert_eq!(&buf, b"efgh");
    assert_eq!(file.offset(), 8);
    assert_eq!(file.read(&mut buf), Ok(2));
    assert_eq!(&buf[..2], b"ij");
    assert_eq!(file.offset(), 10);
    assert_eq!(file.read(&mut buf), Ok(0), "end of file");
    assert_eq!(file.offset(), 10);
}

#[test]
fn a_short_read_advances_the_offset_by_the_bytes_it_got() {
    let (ns, ctx) = fresh();
    let opened = ns.open(&ctx, None, b"/f", &RW_CREATE, 0o644).unwrap();
    let file = opened
        .with_io(Watched::new(b"0123456789", 3) as Arc<dyn crate::Inode>)
        .unwrap();
    let mut buf = [0_u8; 8];
    assert_eq!(file.read(&mut buf), Ok(3));
    assert_eq!(&buf[..3], b"012");
    assert_eq!(file.offset(), 3);
    assert_eq!(file.read(&mut buf), Ok(3));
    assert_eq!(
        &buf[..3],
        b"345",
        "the second read began where the first stopped"
    );
    assert_eq!(file.offset(), 6);
}

#[test]
fn an_append_writes_at_the_end_and_leaves_the_offset_there() {
    let (ns, ctx) = fresh();
    let plain = ns.open(&ctx, None, b"/log", &RW_CREATE, 0o644).unwrap();
    let appending = ns.open(&ctx, None, b"/log", &APPEND_CREATE, 0o644).unwrap();
    assert_eq!(plain.write(b"12345"), Ok(5));
    assert_eq!(appending.write(b"ab"), Ok(2));
    assert_eq!(appending.offset(), 7);
    // The other description overwrites from its own offset and lengthens the
    // file; the append follows the end, wherever its own offset was put.
    assert_eq!(plain.write(b"xyz"), Ok(3));
    assert_eq!(appending.seek(0, Whence::Set), Ok(0));
    assert_eq!(appending.write(b"!"), Ok(1));
    assert_eq!(appending.offset(), 9);
    let mut all = [0_u8; 16];
    assert_eq!(appending.read_at(0, &mut all), Ok(9));
    assert_eq!(&all[..9], b"12345xyz!");
}

// -- The page cache over a source -------------------------------------------

use crate::tmpfs::{HeapPages, MAX_FILL_RUN, PAGE_SIZE, PageSource, Pages, Storage};

const PAGE: usize = PAGE_SIZE as usize;

/// The byte a [`Pattern`] source serves at `offset` into the file: different
/// across a page and from one page to the next.
fn pattern_at(offset: u64) -> u8 {
    let page = offset / PAGE_SIZE;
    let within = offset % PAGE_SIZE;
    (page.wrapping_mul(131).wrapping_add(within) % 251) as u8
}

/// What a [`Pattern`] source serves for `len` bytes at `offset`.
fn pattern(offset: u64, len: usize) -> Vec<u8> {
    (offset..offset + len as u64).map(pattern_at).collect()
}

/// A source serving [`pattern_at`], which remembers every call and can be
/// told to fill short, fail, or lie about what it filled.
#[derive(Debug, Default)]
struct Pattern {
    /// `(first, pages asked for)`, a call at a time.
    asked: ferrix_sync::SpinLock<Vec<(u64, usize)>>,
    /// Fill at most this many pages a call; zero for as many as asked.
    most: AtomicUsize,
    /// Fail every call with `EIO` while set.
    failing: AtomicBool,
    /// Claim this many pages were filled, whatever was.
    claim: ferrix_sync::SpinLock<Option<usize>>,
}

impl Pattern {
    fn asked(&self) -> Vec<(u64, usize)> {
        self.asked.lock().clone()
    }
}

impl PageSource for Pattern {
    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> Result<usize, Errno> {
        self.asked.lock().push((first, pages.len()));
        if self.failing.load(Ordering::Relaxed) {
            return Err(Errno::EIO);
        }
        let most = self.most.load(Ordering::Relaxed);
        let fill = if most == 0 {
            pages.len()
        } else {
            most.min(pages.len())
        };
        // Every page handed over must be a page long; a different error
        // number than the tests expect makes a wrong-sized buffer fail them.
        if pages.iter().any(|page| page.len() != PAGE) {
            return Err(Errno::EINVAL);
        }
        for (index, page) in (first..).zip(pages.iter_mut().take(fill)) {
            page.copy_from_slice(&pattern(index * PAGE_SIZE, PAGE));
        }
        Ok(self.claim.lock().unwrap_or(fill))
    }
}

fn over(source: &Arc<Pattern>) -> HeapPages {
    HeapPages::with_source(Arc::clone(source) as Arc<dyn PageSource>)
}

#[test]
fn a_long_read_asks_the_source_in_runs_of_at_most_32_pages() {
    let source = Arc::new(Pattern::default());
    let pages = over(&source);
    assert_eq!(pages.committed_bytes(), 0, "nothing is held before a read");
    let mut buf = vec![0_u8; 40 * PAGE];
    pages.read(0, &mut buf).unwrap();
    assert!(
        buf == pattern(0, 40 * PAGE),
        "a filled read has the source's bytes"
    );
    assert_eq!(MAX_FILL_RUN, 32);
    assert_eq!(source.asked(), [(0, 32), (32, 8)]);
    assert_eq!(pages.committed_bytes(), 40 * PAGE_SIZE);
    pages.read(0, &mut buf).unwrap();
    assert_eq!(source.asked().len(), 2, "held pages were asked for again");
}

#[test]
fn a_fill_short_of_the_run_is_asked_again_for_the_rest() {
    let source = Arc::new(Pattern::default());
    source.most.store(3, Ordering::Relaxed);
    let pages = over(&source);
    let len = 8 * PAGE - 200;
    let mut buf = vec![0_u8; len];
    pages.read(100, &mut buf).unwrap();
    assert!(
        buf == pattern(100, len),
        "short fills put bytes in the wrong place"
    );
    assert_eq!(source.asked(), [(0, 8), (3, 5), (6, 2)]);
    assert_eq!(pages.committed_bytes(), 8 * PAGE_SIZE);
}

#[test]
fn a_failed_fill_keeps_nothing_and_a_later_read_succeeds() {
    let source = Arc::new(Pattern::default());
    source.failing.store(true, Ordering::Relaxed);
    let pages = over(&source);
    let mut buf = vec![0_u8; 2 * PAGE];
    assert_eq!(pages.read(PAGE_SIZE, &mut buf), Err(Errno::EIO));
    assert_eq!(pages.committed_bytes(), 0, "a failed fill kept pages");
    source.failing.store(false, Ordering::Relaxed);
    assert_eq!(pages.read(PAGE_SIZE, &mut buf), Ok(()));
    assert!(buf == pattern(PAGE_SIZE, 2 * PAGE));
}

#[test]
fn a_source_claiming_no_page_or_too_many_is_an_error() {
    for claim in [0, 3] {
        let source = Arc::new(Pattern::default());
        *source.claim.lock() = Some(claim);
        let pages = over(&source);
        let mut buf = vec![0_u8; 2 * PAGE];
        assert_eq!(
            pages.read(0, &mut buf),
            Err(Errno::EIO),
            "a claim of {claim}"
        );
        assert_eq!(pages.committed_bytes(), 0, "a claim of {claim} kept pages");
        assert_eq!(pages.write(10, b"x"), Err(Errno::EIO), "a claim of {claim}");
        assert_eq!(pages.committed_bytes(), 0, "a claim of {claim} kept pages");
    }
}

#[test]
fn a_partial_write_keeps_the_sources_other_bytes() {
    let source = Arc::new(Pattern::default());
    let pages = over(&source);
    pages.write(PAGE_SIZE + 10, b"hello").unwrap();
    assert_eq!(source.asked(), [(1, 1)]);
    let mut page = vec![0_u8; PAGE];
    pages.read(PAGE_SIZE, &mut page).unwrap();
    let mut expected = pattern(PAGE_SIZE, PAGE);
    expected[10..15].copy_from_slice(b"hello");
    assert!(
        page == expected,
        "a partial write lost the page's other bytes"
    );
    assert_eq!(source.asked().len(), 1, "a written page was filled again");

    // A write across the boundary of two missing pages fills each.
    pages.write(3 * PAGE_SIZE - 2, b"abcd").unwrap();
    assert_eq!(source.asked(), [(1, 1), (2, 1), (3, 1)]);
}

#[test]
fn a_whole_page_write_does_not_ask_the_source() {
    let source = Arc::new(Pattern::default());
    let pages = over(&source);
    pages.write(3 * PAGE_SIZE, &vec![0xaa; PAGE]).unwrap();
    let mut page = vec![0_u8; PAGE];
    pages.read(3 * PAGE_SIZE, &mut page).unwrap();
    assert!(page.iter().all(|&byte| byte == 0xaa));
    assert!(
        source.asked().is_empty(),
        "a whole-page write asked the source"
    );
}

#[test]
fn a_truncated_file_reads_zeros_past_the_cut_when_it_grows() {
    // The cut falls in a page nothing has read.
    let source = Arc::new(Pattern::default());
    let pages = over(&source);
    let cut = PAGE_SIZE + 100;
    pages.discard_from(cut);
    let mut buf = vec![0_u8; 3 * PAGE];
    pages.read(0, &mut buf).unwrap();
    let mut expected = pattern(0, 3 * PAGE);
    expected[cut as usize..].fill(0);
    assert!(buf == expected, "the source's bytes past a cut came back");
    assert_eq!(
        source.asked(),
        [(0, 2)],
        "a page wholly past the cut was asked for"
    );

    // The cut falls in a page already held, and a partial write lands past it.
    let source = Arc::new(Pattern::default());
    let pages = over(&source);
    pages.read(0, &mut buf).unwrap();
    let cut = 2 * PAGE_SIZE + 50;
    pages.discard_from(cut);
    pages.write(5 * PAGE_SIZE + 1, b"z").unwrap();
    let mut grown = vec![0_u8; 6 * PAGE];
    pages.read(0, &mut grown).unwrap();
    let mut expected = pattern(0, 6 * PAGE);
    expected[cut as usize..].fill(0);
    expected[5 * PAGE + 1] = b'z';
    assert!(grown == expected, "the source's bytes past a cut came back");
    assert_eq!(
        source.asked(),
        [(0, 3)],
        "pages past the cut were asked for"
    );
}

#[test]
fn fill_asks_fill_range_for_the_one_page() {
    let source = Pattern::default();
    let mut page = vec![0_u8; PAGE];
    source.fill(5, &mut page).unwrap();
    assert!(page == pattern(5 * PAGE_SIZE, PAGE));
    assert_eq!(source.asked(), [(5, 1)]);
    *source.claim.lock() = Some(2);
    assert_eq!(source.fill(6, &mut page), Err(Errno::EIO));
    *source.claim.lock() = None;
    source.failing.store(true, Ordering::Relaxed);
    assert_eq!(source.fill(7, &mut page), Err(Errno::EIO));
}

/// A store that cannot fill from a source.
#[derive(Debug)]
struct Unsourced;

impl Storage for Unsourced {
    fn allocate(&self) -> Result<alloc::boxed::Box<dyn Pages>, Errno> {
        HeapStorage::new(1 << 20).allocate()
    }
    fn max_file_size(&self) -> u64 {
        1 << 20
    }
}

#[test]
fn a_store_fills_from_a_source_only_where_it_can() {
    let source = Arc::new(Pattern::default()) as Arc<dyn PageSource>;
    assert_eq!(
        Unsourced.allocate_with(Arc::clone(&source)).err(),
        Some(Errno::ENODEV)
    );
    let heap = HeapStorage::new(1 << 20);
    let pages = heap.allocate_with(source).unwrap();
    let mut buf = [0_u8; 16];
    pages.read(PAGE_SIZE - 8, &mut buf).unwrap();
    assert_eq!(buf.to_vec(), pattern(PAGE_SIZE - 8, 16));
    assert!(pages.object().is_none(), "a heap store has nothing to map");
    assert!(heap.allocate().unwrap().object().is_none());
}

#[test]
#[cfg_attr(
    miri,
    ignore = "threads under Miri take minutes, and the tests above are deterministic"
)]
fn two_readers_of_the_same_missing_pages_both_see_the_sources_bytes() {
    extern crate std;

    let source = Arc::new(Pattern::default());
    source.most.store(5, Ordering::Relaxed);
    for _ in 0..20 {
        let pages = Arc::new(over(&source));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let pages = Arc::clone(&pages);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut buf = vec![0_u8; 40 * PAGE];
                    let _ = barrier.wait();
                    pages.read(3, &mut buf).unwrap();
                    buf == pattern(3, 40 * PAGE)
                })
            })
            .collect();
        for reader in readers {
            assert!(
                reader.join().unwrap(),
                "a racing reader saw the wrong bytes"
            );
        }
        assert_eq!(pages.committed_bytes(), 41 * PAGE_SIZE);
    }
}

// -- Socket buffers ----------------------------------------------------------

use ferrix_linux_abi::socket::SOCKET_BUFFER_MIN;

use crate::socket::{Kind, ReadOutcome as SocketRead, SocketBuffer, WriteOutcome as SocketWrite};

/// The smallest capacity a socket buffer has, which is what these tests use
/// so a full buffer is a page rather than 200 KiB.
const SOCKET_CAP: usize = SOCKET_BUFFER_MIN;

/// Read or peek into a buffer larger than anything queued, and keep the bytes.
fn read_all(buf: &mut SocketBuffer<u32>, peek: bool) -> (SocketRead<u32>, Vec<u8>) {
    let mut out = vec![0_u8; 2 * SOCKET_CAP];
    let outcome = buf.read(&mut out, peek);
    let len = match outcome {
        SocketRead::Read { bytes, .. } => bytes,
        _ => 0,
    };
    out.truncate(len);
    (outcome, out)
}

fn got(bytes: usize, full: usize, ancillary: Option<u32>) -> SocketRead<u32> {
    SocketRead::Read {
        bytes,
        full,
        ancillary,
    }
}

#[test]
fn a_stream_joins_plain_writes_into_one_read() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    for chunk in [&b"ab"[..], b"cd", b"e"] {
        assert_eq!(buf.write(chunk, &mut None), SocketWrite::Wrote(chunk.len()));
    }
    assert_eq!(buf.queued(), 5);
    assert_eq!(buf.next_record(), Some(5), "a stream is one record");
    assert_eq!(
        read_all(&mut buf, false),
        (got(5, 5, None), b"abcde".to_vec())
    );
    assert_eq!(buf.queued(), 0);
    assert_eq!(buf.next_record(), None);
}

#[test]
fn a_stream_write_takes_what_fits_and_a_full_one_would_block() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    let most = vec![1_u8; SOCKET_CAP - 3];
    assert_eq!(
        buf.write(&most, &mut None),
        SocketWrite::Wrote(SOCKET_CAP - 3)
    );
    assert!(buf.can_write(10), "a stream needs room for one byte");
    assert_eq!(buf.write(b"0123456789", &mut None), SocketWrite::Wrote(3));
    assert!(!buf.can_write(1));
    assert!(buf.can_write(0));
    assert_eq!(buf.write(b"x", &mut None), SocketWrite::WouldBlock);
    assert_eq!(buf.write(b"", &mut None), SocketWrite::Wrote(0));
    let mut two = [0_u8; 2];
    assert_eq!(buf.read(&mut two, false), got(2, 2, None));
    assert!(buf.can_write(1));
    assert_eq!(buf.write(b"xyz", &mut None), SocketWrite::Wrote(2));
    assert_eq!(buf.queued(), SOCKET_CAP);
}

#[test]
fn shrinking_a_socket_buffer_blocks_writes_until_it_drains() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, 2 * SOCKET_CAP);
    let data = vec![3_u8; SOCKET_CAP + 10];
    assert_eq!(buf.write(&data, &mut None), SocketWrite::Wrote(data.len()));
    buf.set_capacity(SOCKET_CAP);
    assert_eq!(buf.queued(), SOCKET_CAP + 10, "shrinking discards nothing");
    assert_eq!(buf.write(b"x", &mut None), SocketWrite::WouldBlock);
    let mut ten = [0_u8; 10];
    assert_eq!(buf.read(&mut ten, false), got(10, 10, None));
    assert_eq!(buf.write(b"x", &mut None), SocketWrite::WouldBlock);
    assert_eq!(buf.read(&mut ten[..1], false), got(1, 1, None));
    assert_eq!(buf.write(b"xy", &mut None), SocketWrite::Wrote(1));
    buf.set_capacity(1);
    assert_eq!(buf.capacity(), SOCKET_BUFFER_MIN, "the floor holds");
    assert_eq!(
        SocketBuffer::<u32>::new(Kind::Record, 0).capacity(),
        SOCKET_BUFFER_MIN
    );
}

/// A read runs on through plain bytes into bytes that bring descriptors,
/// takes them, and stops at the end of those bytes. Measured on a 7.0 host:
/// a socketpair holding 100 plain bytes, 100 sent with a descriptor and 100
/// plain reads as 200 and then 100 -- by `read`, by `recvmsg` with a control
/// buffer and without one, and by `MSG_PEEK` -- and 100 plain, 100 with a
/// descriptor and 100 with another reads as 200, then 100.
#[test]
fn a_stream_read_runs_into_ancillary_data_and_stops_after_it() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    assert_eq!(buf.write(b"plain", &mut None), SocketWrite::Wrote(5));
    assert_eq!(buf.write(b"rights", &mut Some(1)), SocketWrite::Wrote(6));
    assert_eq!(buf.write(b"after", &mut None), SocketWrite::Wrote(5));
    assert_eq!(buf.write(b"more", &mut Some(2)), SocketWrite::Wrote(4));
    assert_eq!(buf.write(b"most", &mut Some(3)), SocketWrite::Wrote(4));
    assert_eq!(
        read_all(&mut buf, false),
        (got(11, 11, Some(1)), b"plainrights".to_vec()),
        "a read runs on into bytes that bring descriptors, and not past them"
    );
    assert_eq!(
        read_all(&mut buf, false),
        (got(9, 9, Some(2)), b"aftermore".to_vec())
    );
    assert_eq!(
        read_all(&mut buf, false),
        (got(4, 4, Some(3)), b"most".to_vec()),
        "one set of descriptors per read"
    );
    assert_eq!(buf.read(&mut [0_u8; 8], false), SocketRead::WouldBlock);
}

/// Ancillary data a caller picks with `stopping_before` is a boundary on
/// both sides: a read that has taken bytes stops before it, and one that
/// starts on it takes it.
#[test]
fn a_stream_read_stops_before_what_the_caller_picks() {
    let mut buf =
        SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP).stopping_before(|&stamp| stamp >= 100);
    assert_eq!(buf.write(b"plain", &mut None), SocketWrite::Wrote(5));
    assert_eq!(buf.write(b"stamped", &mut Some(100)), SocketWrite::Wrote(7));
    assert_eq!(buf.write(b"rights", &mut Some(1)), SocketWrite::Wrote(6));
    assert_eq!(
        read_all(&mut buf, true),
        (got(5, 5, None), b"plain".to_vec())
    );
    let mut three = [0_u8; 3];
    assert_eq!(buf.read(&mut three, false), got(3, 3, None));
    assert_eq!(buf.read_on(&mut three), (2, None));
    assert_eq!(buf.read_on(&mut three), (0, None), "not into the stamp");
    assert_eq!(
        read_all(&mut buf, false),
        (got(7, 7, Some(100)), b"stamped".to_vec()),
        "a read that starts on it takes it"
    );
    assert_eq!(
        read_all(&mut buf, false),
        (got(6, 6, Some(1)), b"rights".to_vec())
    );
}

#[test]
fn ancillary_data_comes_with_the_first_byte_of_a_segment_read_in_pieces() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    assert_eq!(buf.write(b"rights", &mut Some(1)), SocketWrite::Wrote(6));
    assert_eq!(buf.write(b"tail", &mut None), SocketWrite::Wrote(4));
    let mut zero = [0_u8; 0];
    assert_eq!(
        buf.read(&mut zero, false),
        got(0, 0, None),
        "a zero-length read takes nothing, ancillary data included"
    );
    let mut two = [0_u8; 2];
    assert_eq!(buf.read(&mut two, false), got(2, 2, Some(1)));
    assert_eq!(&two, b"ri");
    assert_eq!(
        read_all(&mut buf, false),
        (got(8, 8, None), b"ghtstail".to_vec()),
        "once its data is taken the rest is ordinary bytes"
    );

    // And a read that ends inside the bytes that brought descriptors after
    // plain ones takes the descriptors with it. Measured on a 7.0 host:
    // 100 plain, 100 with a descriptor, 100 plain, read for 150 is 150 with
    // the descriptor, then 150.
    assert_eq!(buf.write(b"ab", &mut None), SocketWrite::Wrote(2));
    assert_eq!(buf.write(b"cd", &mut Some(2)), SocketWrite::Wrote(2));
    assert_eq!(buf.write(b"ef", &mut None), SocketWrite::Wrote(2));
    let mut three = [0_u8; 3];
    assert_eq!(buf.read(&mut three, false), got(3, 3, Some(2)));
    assert_eq!(&three, b"abc");
    assert_eq!(
        read_all(&mut buf, false),
        (got(3, 3, None), b"def".to_vec())
    );
}

/// A read that goes on in pieces takes what one read into a larger buffer
/// would: through plain writes, and up to the boundaries ancillary data
/// makes. Measured on a 7.0 host: `readv` of a socketpair holding six bytes
/// into two segments of four is 6, and a read of 65536 bytes from one
/// holding 8000 bytes sent with a descriptor and then 100 is 8000.
#[test]
fn a_stream_read_goes_on_in_pieces_up_to_the_boundaries_one_read_keeps() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    assert_eq!(buf.write(b"abcdef", &mut None), SocketWrite::Wrote(6));
    let mut four = [0_u8; 4];
    assert_eq!(buf.read(&mut four, false), got(4, 4, None));
    assert_eq!(
        buf.read_on(&mut four),
        (2, None),
        "the rest of what is there"
    );
    assert_eq!(&four[..2], b"ef");
    assert_eq!(
        buf.read_on(&mut four),
        (0, None),
        "and nothing is waited for"
    );

    // Plain writes run on into each other.
    for chunk in [&b"ab"[..], b"cd", b"ef"] {
        assert_eq!(buf.write(chunk, &mut None), SocketWrite::Wrote(2));
    }
    assert_eq!(buf.read(&mut four[..1], false), got(1, 1, None));
    assert_eq!(buf.read_on(&mut four), (4, None));
    assert_eq!(&four, b"bcde");
    assert_eq!(buf.read_on(&mut four), (1, None));
    assert_eq!(buf.queued(), 0);

    // Bytes that brought a descriptor are read to their end and no further,
    // however many pieces that takes.
    assert_eq!(buf.write(b"rightsxx", &mut Some(1)), SocketWrite::Wrote(8));
    assert_eq!(buf.write(b"after", &mut None), SocketWrite::Wrote(5));
    let mut two = [0_u8; 2];
    assert_eq!(buf.read(&mut two, false), got(2, 2, Some(1)));
    assert_eq!(buf.read_on(&mut four), (4, None));
    assert_eq!(&four, b"ghts");
    assert_eq!(buf.read_on(&mut four), (2, None), "the last of the segment");
    assert_eq!(&four[..2], b"xx");
    assert_eq!(
        buf.read_on(&mut four),
        (0, None),
        "and not into what came after"
    );
    assert_eq!(
        read_all(&mut buf, false),
        (got(5, 5, None), b"after".to_vec()),
        "which the next read takes"
    );

    // Into bytes that bring descriptors after plain ones, taking them, and
    // not past them: a `read` has nowhere to put them, and its caller drops
    // them.
    assert_eq!(buf.write(b"plain", &mut None), SocketWrite::Wrote(5));
    assert_eq!(buf.write(b"fdfd", &mut Some(2)), SocketWrite::Wrote(4));
    assert_eq!(buf.write(b"next", &mut None), SocketWrite::Wrote(4));
    assert_eq!(buf.read(&mut four, false), got(4, 4, None));
    assert_eq!(buf.read_on(&mut four[..2]), (2, Some(2)));
    assert_eq!(&four[..2], b"nf");
    assert_eq!(buf.read_on(&mut four), (3, None), "the rest of those bytes");
    assert_eq!(&four[..3], b"dfd");
    assert_eq!(buf.read_on(&mut four), (0, None), "and not past them");
    assert_eq!(
        read_all(&mut buf, false),
        (got(4, 4, None), b"next".to_vec())
    );

    // A read of a whole segment with its descriptor ends there too.
    assert_eq!(buf.write(b"fd", &mut Some(3)), SocketWrite::Wrote(2));
    assert_eq!(buf.write(b"next", &mut None), SocketWrite::Wrote(4));
    assert_eq!(buf.read(&mut four, false), got(2, 2, Some(3)));
    assert_eq!(buf.read_on(&mut four), (0, None));
    assert_eq!(buf.read(&mut four, false), got(4, 4, None));

    // A buffer of records is never read on: one read is one record.
    let mut records = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    assert_eq!(records.write(b"one", &mut None), SocketWrite::Wrote(3));
    assert_eq!(records.write(b"two", &mut None), SocketWrite::Wrote(3));
    assert_eq!(records.read(&mut four, false), got(3, 3, None));
    assert_eq!(records.read_on(&mut four), (0, None));
    assert_eq!(records.queued(), 3);
}

#[test]
fn ancillary_data_stays_with_the_writer_unless_it_is_queued() {
    let mut stream = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    let full = vec![0_u8; SOCKET_CAP];
    assert_eq!(
        stream.write(&full, &mut None),
        SocketWrite::Wrote(SOCKET_CAP)
    );
    let mut rights = Some(7);
    assert_eq!(stream.write(b"x", &mut rights), SocketWrite::WouldBlock);
    assert_eq!(rights, Some(7), "lost on WouldBlock");
    assert_eq!(stream.write(b"", &mut rights), SocketWrite::Wrote(0));
    assert_eq!(rights, Some(7), "lost on an empty stream write");
    assert_eq!(stream.read(&mut [0_u8; 1], false), got(1, 1, None));
    assert_eq!(stream.write(b"xy", &mut rights), SocketWrite::Wrote(1));
    assert_eq!(rights, None, "one byte accepted takes it");
    assert_eq!(stream.close_reader(), [7], "and it is handed back on close");
    let mut rights = Some(8);
    assert_eq!(stream.write(b"x", &mut rights), SocketWrite::Broken);
    assert_eq!(rights, Some(8), "lost on Broken to a stream");

    let mut record = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    let too_big = vec![0_u8; SOCKET_CAP + 1];
    assert_eq!(record.write(&too_big, &mut rights), SocketWrite::TooBig);
    assert_eq!(rights, Some(8), "lost on TooBig");
    assert_eq!(
        record.write(&full, &mut None),
        SocketWrite::Wrote(SOCKET_CAP)
    );
    assert_eq!(record.write(b"", &mut rights), SocketWrite::WouldBlock);
    assert_eq!(rights, Some(8), "lost on WouldBlock to a record");
    assert!(record.close_reader().is_empty());
    assert_eq!(record.write(b"r", &mut rights), SocketWrite::Broken);
    assert_eq!(rights, Some(8), "lost on Broken to a record");
    assert_eq!(record.write(&too_big, &mut rights), SocketWrite::TooBig);
}

#[test]
fn a_record_read_takes_one_record_and_says_how_long_it_was() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    assert_eq!(
        buf.write(b"first record", &mut Some(1)),
        SocketWrite::Wrote(12)
    );
    assert_eq!(buf.write(b"second", &mut None), SocketWrite::Wrote(6));
    assert_eq!(buf.next_record(), Some(12));
    assert_eq!(buf.queued(), 18);
    let mut five = [0_u8; 5];
    assert_eq!(buf.read(&mut five, false), got(5, 12, Some(1)));
    assert_eq!(&five, b"first");
    assert_eq!(
        read_all(&mut buf, false),
        (got(6, 6, None), b"second".to_vec()),
        "the rest of a cut-short record is discarded"
    );
    assert_eq!(buf.queued(), 0);
    assert_eq!(buf.read(&mut five, false), SocketRead::WouldBlock);
}

#[test]
fn a_record_goes_in_whole_or_not_at_all() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    let most = vec![1_u8; SOCKET_CAP - 4];
    assert_eq!(buf.write(&most, &mut None), SocketWrite::Wrote(most.len()));
    assert!(!buf.can_write(5));
    assert_eq!(buf.write(b"12345", &mut None), SocketWrite::WouldBlock);
    assert_eq!(buf.queued(), most.len(), "a refused record adds nothing");
    assert!(buf.can_write(4));
    assert_eq!(buf.write(b"1234", &mut None), SocketWrite::Wrote(4));
    assert!(
        buf.can_write(SOCKET_CAP + 1),
        "a record that can never fit must not wait"
    );
    assert_eq!(
        buf.write(&vec![0_u8; SOCKET_CAP + 1], &mut None),
        SocketWrite::TooBig
    );
}

#[test]
fn empty_records_are_records_and_run_out_of_room() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    assert_eq!(buf.write(b"ab", &mut None), SocketWrite::Wrote(2));
    assert_eq!(buf.write(b"", &mut Some(5)), SocketWrite::Wrote(0));
    assert_eq!(buf.write(b"cd", &mut None), SocketWrite::Wrote(2));
    assert_eq!(buf.queued(), 4);
    assert_eq!(read_all(&mut buf, false), (got(2, 2, None), b"ab".to_vec()));
    assert_eq!(buf.next_record(), Some(0));
    assert!(buf.can_read(), "an empty record is something to read");
    assert_eq!(read_all(&mut buf, false), (got(0, 0, Some(5)), Vec::new()));
    assert_eq!(read_all(&mut buf, false), (got(2, 2, None), b"cd".to_vec()));
    assert!(!buf.can_read());

    let mut count = 0;
    while buf.write(b"", &mut None) == SocketWrite::Wrote(0) {
        count += 1;
        assert!(count <= SOCKET_CAP, "empty records were free");
    }
    assert_eq!(count, SOCKET_CAP, "each empty record costs one byte");
    assert_eq!(buf.queued(), 0, "but queues no payload");
    assert!(!buf.can_write(0));
}

#[test]
fn a_peek_copies_what_a_read_would_take_and_takes_nothing() {
    let mut stream = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    assert_eq!(stream.write(b"plain", &mut None), SocketWrite::Wrote(5));
    assert_eq!(stream.write(b"rights", &mut Some(3)), SocketWrite::Wrote(6));
    assert_eq!(stream.write(b"after", &mut None), SocketWrite::Wrote(5));
    for _ in 0..2 {
        assert_eq!(
            read_all(&mut stream, true),
            (got(11, 11, None), b"plainrights".to_vec()),
            "a peek returns no ancillary data"
        );
    }
    assert_eq!(
        read_all(&mut stream, false),
        (got(11, 11, Some(3)), b"plainrights".to_vec()),
        "and leaves it for the read"
    );
    assert_eq!(
        read_all(&mut stream, true),
        (got(5, 5, None), b"after".to_vec())
    );

    let mut record = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    assert_eq!(record.write(b"record", &mut Some(4)), SocketWrite::Wrote(6));
    let mut three = [0_u8; 3];
    assert_eq!(record.read(&mut three, true), got(3, 6, None));
    assert_eq!(&three, b"rec");
    assert_eq!(record.next_record(), Some(6), "a peek takes no record");
    assert_eq!(
        read_all(&mut record, false),
        (got(6, 6, Some(4)), b"record".to_vec())
    );
}

#[test]
fn end_of_file_comes_only_once_a_closed_writers_data_is_read() {
    for kind in [Kind::Stream, Kind::Record] {
        let mut buf = SocketBuffer::<u32>::new(kind, SOCKET_CAP);
        assert_eq!(buf.read(&mut [0_u8; 8], false), SocketRead::WouldBlock);
        assert!(!buf.can_read());
        assert_eq!(buf.write(b"last", &mut Some(1)), SocketWrite::Wrote(4));
        buf.close_writer();
        assert!(buf.writer_closed() && !buf.reader_closed());
        assert!(buf.can_read());
        assert_eq!(
            read_all(&mut buf, false),
            (got(4, 4, Some(1)), b"last".to_vec()),
            "{kind:?}: queued data outlives the writer"
        );
        for _ in 0..2 {
            assert_eq!(buf.read(&mut [0_u8; 8], false), SocketRead::EndOfFile);
            assert_eq!(buf.read(&mut [0_u8; 8], true), SocketRead::EndOfFile);
        }
        assert!(buf.can_read(), "{kind:?}: end of file must wake a reader");
    }
}

#[test]
fn a_closed_reader_breaks_writes_and_hands_back_what_was_queued() {
    for kind in [Kind::Stream, Kind::Record] {
        let mut buf = SocketBuffer::<u32>::new(kind, SOCKET_CAP);
        assert_eq!(buf.write(b"one", &mut Some(1)), SocketWrite::Wrote(3));
        assert_eq!(buf.write(b"two", &mut None), SocketWrite::Wrote(3));
        assert_eq!(buf.write(b"three", &mut Some(3)), SocketWrite::Wrote(5));
        let dropped = buf.close_reader();
        assert_eq!(dropped, [1, 3], "{kind:?}");
        assert!(buf.reader_closed());
        assert_eq!(buf.queued(), 0);
        assert!(buf.can_write(1) && buf.can_write(2 * SOCKET_CAP));
        assert_eq!(buf.write(b"x", &mut None), SocketWrite::Broken, "{kind:?}");
        assert_eq!(buf.write(b"", &mut None), SocketWrite::Broken, "{kind:?}");
    }
}

#[test]
fn drain_returns_every_ancillary_value_in_order_and_empties_the_buffer() {
    let mut stream = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    for (data, ancillary) in [
        (&b"a"[..], Some(1)),
        (b"b", None),
        (b"c", Some(2)),
        (b"d", Some(3)),
    ] {
        let mut ancillary = ancillary;
        assert_eq!(stream.write(data, &mut ancillary), SocketWrite::Wrote(1));
    }
    assert_eq!(stream.read(&mut [0_u8; 1], false), got(1, 1, Some(1)));
    assert_eq!(stream.drain(), [2, 3]);
    assert_eq!(stream.queued(), 0);
    assert_eq!(stream.read(&mut [0_u8; 1], false), SocketRead::WouldBlock);
    let full = vec![0_u8; SOCKET_CAP];
    assert_eq!(
        stream.write(&full, &mut None),
        SocketWrite::Wrote(SOCKET_CAP),
        "a drained buffer has all its room back"
    );

    let mut record = SocketBuffer::<u32>::new(Kind::Record, SOCKET_CAP);
    for ancillary in [Some(10), None, Some(20), Some(30)] {
        let mut ancillary = ancillary;
        assert_eq!(record.write(b"r", &mut ancillary), SocketWrite::Wrote(1));
    }
    assert_eq!(record.drain(), [10, 20, 30]);
    assert_eq!(record.next_record(), None);
    assert!(record.drain().is_empty());
}

#[test]
fn a_stream_delivers_bytes_and_ancillary_data_in_order_against_a_model() {
    let mut buf = SocketBuffer::<u64>::new(Kind::Stream, SOCKET_CAP);
    // Every queued byte, the step that wrote it, and on the first byte of a
    // write that brought ancillary data, that data.
    let mut model = alloc::collections::VecDeque::<(u8, usize, Option<u64>)>::new();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    // As for the pipe's model: 6000 steps took Miri hours, 300 still fill
    // the buffer and wrap it.
    let steps = if cfg!(miri) { 300 } else { 6000 };
    for step in 0..steps {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = usize::try_from((state >> 8) % 700).unwrap();
        if state & 1 == 0 {
            let data: Vec<u8> = (0..len).map(|i| (step + i) as u8).collect();
            let offered = (state & 6 == 0).then_some(state);
            let mut ancillary = offered;
            match buf.write(&data, &mut ancillary) {
                SocketWrite::Wrote(n) => {
                    assert!(n == data.len() || model.len() + n == SOCKET_CAP);
                    assert_eq!(ancillary, if n > 0 { None } else { offered });
                    for (i, &byte) in data[..n].iter().enumerate() {
                        model.push_back((byte, step, offered.filter(|_| i == 0)));
                    }
                }
                SocketWrite::WouldBlock => {
                    assert_eq!(model.len(), SOCKET_CAP);
                    assert_eq!(ancillary, offered, "ancillary data was lost");
                }
                other => panic!("a reader is open: {other:?}"),
            }
        } else {
            let peek = state & 2 != 0;
            // What Linux's rule says the read takes: across writes, into
            // bytes that bring ancillary data, and not past the write whose
            // ancillary data it took.
            let mut expected = Vec::new();
            let mut taken = None;
            let mut taken_from = None;
            for &(byte, write, carried) in &model {
                if expected.len() == len || taken_from.is_some_and(|from| write != from) {
                    break;
                }
                if carried.is_some() {
                    taken = carried;
                    taken_from = Some(write);
                }
                expected.push(byte);
            }
            let mut out = vec![0_u8; len];
            match buf.read(&mut out, peek) {
                SocketRead::Read {
                    bytes,
                    full,
                    ancillary,
                } => {
                    assert_eq!(bytes, full);
                    assert_eq!(&out[..bytes], &expected[..], "bytes came out wrong");
                    if peek {
                        assert_eq!(ancillary, None);
                    } else {
                        assert_eq!(ancillary, taken, "ancillary data came out wrong");
                        let _ = model.drain(..bytes);
                    }
                }
                SocketRead::WouldBlock => assert!(model.is_empty()),
                SocketRead::EndOfFile => panic!("a writer is open"),
            }
        }
        assert_eq!(buf.queued(), model.len());
    }
}

#[test]
fn a_socket_buffer_shows_its_ancillary_data_without_taking_it() {
    let mut buf = SocketBuffer::<u32>::new(Kind::Stream, SOCKET_CAP);
    assert_eq!(buf.write(b"ab", &mut Some(7)), SocketWrite::Wrote(2));
    assert_eq!(buf.write(b"cd", &mut None), SocketWrite::Wrote(2));
    assert_eq!(buf.write(b"ef", &mut Some(9)), SocketWrite::Wrote(2));
    assert_eq!(buf.ancillary().copied().collect::<Vec<_>>(), [7, 9]);
    // Looking took nothing: the first read still brings the first set.
    let (outcome, bytes) = read_all(&mut buf, false);
    assert_eq!(bytes, b"ab");
    assert!(matches!(
        outcome,
        SocketRead::Read {
            ancillary: Some(7),
            ..
        }
    ));
    assert_eq!(buf.ancillary().copied().collect::<Vec<_>>(), [9]);
}

// -- Magic links -----------------------------------------------------------

/// A directory of magic links, as `/proc/<pid>` holds `exe`: each name stands
/// for a location, and reads as a path that leads nowhere, so a walk that
/// followed the text instead of the object would fail.
#[derive(Debug, Default)]
struct Magic {
    links: ferrix_sync::SpinLock<Vec<(Vec<u8>, Option<Location>)>>,
}

/// One of [`Magic`]'s links.
#[derive(Debug)]
struct MagicLink(Option<Location>);

impl crate::Inode for MagicLink {
    fn metadata(&self) -> crate::Metadata {
        Volatile::meta(3, FileType::Symlink)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn read_link(&self) -> Result<Vec<u8>, Errno> {
        Ok(b"/nowhere".to_vec())
    }

    fn link_location(&self) -> Option<Result<Location, Errno>> {
        Some(self.0.clone().ok_or(Errno::ENOENT))
    }
}

impl crate::Inode for Magic {
    fn metadata(&self) -> crate::Metadata {
        Volatile::meta(1, FileType::Directory)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn caches_lookups(&self) -> bool {
        false
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn crate::Inode>, Errno> {
        let links = self.links.lock();
        let (_, target) = links
            .iter()
            .find(|(held, _)| held == name)
            .ok_or(Errno::ENOENT)?;
        Ok(Arc::new(MagicLink(target.clone())))
    }
}

#[derive(Debug)]
struct MagicFs(Arc<Magic>);

impl FileSystem for MagicFs {
    fn root(&self) -> Arc<dyn crate::Inode> {
        Arc::clone(&self.0) as Arc<dyn crate::Inode>
    }

    fn name(&self) -> &'static str {
        "magic"
    }

    fn device(&self) -> u64 {
        10
    }
}

#[test]
fn a_magic_link_leads_to_its_object_and_not_to_its_text() {
    let (ns, ctx) = fresh();
    ns.mkdir(&ctx, None, b"/bin", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/etc", 0o755).unwrap();
    ns.mkdir(&ctx, None, b"/proc", 0o755).unwrap();
    write_file(&ns, &ctx, "/bin/prog", b"the program");
    write_file(&ns, &ctx, "/etc/conf", b"a setting");
    let program = ns.resolve(&ctx, None, b"/bin/prog", true).unwrap();
    let etc = ns.resolve(&ctx, None, b"/etc", true).unwrap();

    let magic = Arc::new(Magic::default());
    magic.links.lock().extend([
        (b"exe".to_vec(), Some(program.clone())),
        (b"cwd".to_vec(), Some(etc)),
        (b"gone".to_vec(), None),
    ]);
    let at = ns.resolve(&ctx, None, b"/proc", true).unwrap();
    let _ = ns.mount(Arc::new(MagicFs(magic)), &at).unwrap();

    let found = ns.resolve(&ctx, None, b"/proc/exe", true).unwrap();
    assert!(found.same(&program), "followed, it is the program's file");
    assert_eq!(read_file(&ns, &ctx, "/proc/exe").unwrap(), b"the program");
    assert_eq!(
        ns.read_link(&ctx, None, b"/proc/exe").unwrap(),
        b"/nowhere".to_vec(),
        "read, it is still the text"
    );
    let link = ns.resolve(&ctx, None, b"/proc/exe", false).unwrap();
    assert!(!link.same(&program), "not followed, it is the link itself");
    assert_eq!(
        ns.stat(&link).unwrap().metadata.kind,
        FileType::Symlink,
        "not followed, it is the link itself"
    );

    // Through a magic link to a directory, the walk goes on from there.
    assert_eq!(
        read_file(&ns, &ctx, "/proc/cwd/conf").unwrap(),
        b"a setting"
    );
    assert_eq!(
        read_file(&ns, &ctx, "/proc/exe/conf").err(),
        Some(Errno::ENOTDIR),
        "a file is not a directory, however it was reached"
    );

    // What the object is, not where it was: gone from its directory, the
    // program is still what the link leads to.
    ns.unlink(&ctx, None, b"/bin/prog").unwrap();
    assert_eq!(read_file(&ns, &ctx, "/bin/prog").err(), Some(Errno::ENOENT));
    assert_eq!(read_file(&ns, &ctx, "/proc/exe").unwrap(), b"the program");

    // A magic link with nothing behind it is its own error, and nothing may
    // be made or removed through one.
    assert_eq!(
        ns.resolve(&ctx, None, b"/proc/gone", true).err(),
        Some(Errno::ENOENT)
    );
    assert!(ns.mkdir(&ctx, None, b"/proc/cwd", 0o755).is_err());
}

// -- Kernel memory charged to the job (certification finding F-37) ----------

mod charged {
    use alloc::format;

    use ferrix_kmem::testing::Job;

    use super::{RW_CREATE, fresh};
    use crate::Errno;
    use crate::fd::FdTable;
    use crate::pipe::{PipeBuffer, WriteOutcome};
    use crate::socket::{self, SocketBuffer};

    /// A job at its limit is refused one more file, with nothing made; a
    /// sibling job makes one; and every byte comes back as the files go.
    #[test]
    fn files_their_names_and_dentries_are_the_makers_and_come_back() {
        let (ns, ctx) = fresh();
        let job = Job::enter(64 * 1024);
        let mut made = 0;
        let refused = loop {
            let path = format!("/f{made}");
            match ns.open(&ctx, None, path.as_bytes(), &RW_CREATE, 0o644) {
                Ok(file) => {
                    drop(file);
                    made += 1;
                }
                Err(errno) => break errno,
            }
        };
        assert_eq!(refused, Errno::ENOMEM);
        assert!(made > 20, "only {made} files in 64 KiB");
        assert!(job.used() <= 64 * 1024 && job.refused() >= 1);
        let path = format!("/f{made}");
        assert_eq!(
            ns.resolve(&ctx, None, path.as_bytes(), true).err(),
            Some(Errno::ENOENT),
            "a refused create left its name behind"
        );
        let sibling = Job::enter(64 * 1024);
        let _ = ns.open(&ctx, None, b"/sibling", &RW_CREATE, 0o644).unwrap();
        assert!(sibling.used() > 0);
        sibling.outside(|| {
            for at in 0..made {
                ns.unlink(&ctx, None, format!("/f{at}").as_bytes()).unwrap();
            }
        });
        drop(sibling);
        // The dentry cache still names the removed files' negative entries,
        // which stay the job's until it lets them go, as Linux's do.
        drop(ns);
        assert_eq!((job.used(), job.holds()), (0, 0));
    }

    /// A pipe's buffer is charged as it grows, to the job that made it, and
    /// refused at its limit with nothing queued.
    #[test]
    fn a_pipe_buffer_grows_within_its_job() {
        let job = Job::enter(16 * 1024);
        let mut pipe = PipeBuffer::new(65536);
        pipe.open_reader();
        pipe.open_writer();
        let page = [7_u8; 4096];
        let mut queued = 0;
        let refusal = loop {
            match pipe.write(&page) {
                WriteOutcome::Wrote(count) => queued += count,
                other => break other,
            }
        };
        assert_eq!(refusal, WriteOutcome::NoMemory);
        assert_eq!(pipe.len(), queued, "a refused write queued something");
        assert_eq!(pipe.charged(), job.used());
        assert!(job.used() <= 16 * 1024);
        drop(pipe);
        assert_eq!((job.used(), job.holds()), (0, 0));
    }

    /// Empty records cost their queue slot, charged to the writer; a record
    /// refused leaves its ancillary data with the caller.
    #[test]
    fn empty_records_are_charged_for_what_they_hold() {
        let job = Job::enter(8 * 1024);
        let mut buffer: SocketBuffer<u32> = SocketBuffer::new(socket::Kind::Record, 1 << 20);
        let mut sent = 0;
        let mut ancillary = Some(9);
        let refusal = loop {
            match buffer.write(b"", &mut ancillary) {
                socket::WriteOutcome::Wrote(_) => {
                    sent += 1;
                    ancillary = Some(9);
                }
                other => break other,
            }
        };
        assert_eq!(refusal, socket::WriteOutcome::NoMemory);
        assert_eq!(
            ancillary,
            Some(9),
            "a refused record took its ancillary data"
        );
        assert!(sent > 10 && sent < 1000, "{sent} empty records in 8 KiB");
        assert_eq!(buffer.charged_heap(), job.used());
        let _ = buffer.drain();
        drop(buffer);
        assert_eq!((job.used(), job.holds()), (0, 0));
    }

    /// `dup2` far up a table grows it to there, charged: refused past the
    /// job's limit, with the table as it was.
    #[test]
    fn a_descriptor_table_is_charged_for_its_highest_number() {
        let job = Job::enter(4096);
        let mut table: FdTable<u32> = FdTable::new();
        table.set_limit(1 << 20).unwrap();
        let _ = table.insert(1, false).unwrap();
        assert_eq!(table.install(100_000, 2, false), Err(Errno::ENOMEM));
        assert_eq!(table.get(100_000), Err(Errno::EBADF));
        assert_eq!(table.install(100, 3, false), Ok(None));
        assert_eq!(table.charged(), job.used());
        let copy = table.try_clone().unwrap();
        assert_eq!(copy.get(100), Ok(&3));
        drop((table, copy));
        assert_eq!((job.used(), job.holds()), (0, 0));
    }
}
