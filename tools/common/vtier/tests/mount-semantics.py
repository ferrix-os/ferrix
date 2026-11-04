#!/usr/bin/env python3
"""What a build asks of a file system, each case named, against a real vtier mount.

    mount-semantics.py <mountpoint> <directory on another file system>

Not a cargo test: `cargo test` needs no /dev/fuse and no card. A store's
qualification runs this (docs/TEST-TIME.md, ledger 6697 V3), and it exits non-zero
if any case fails.
"""
import errno
import fcntl
import mmap
import os
import stat
import subprocess
import sys
import tempfile
import time

mount, other = sys.argv[1], sys.argv[2]
work = tempfile.mkdtemp(prefix="sem.", dir=mount)
outside = tempfile.mkdtemp(prefix="vtier-sem.", dir=other)
failures = 0


def path(name):
    return os.path.join(work, name)


def write(name, data=b"data"):
    with open(path(name), "wb") as f:
        f.write(data)


def read(name):
    with open(path(name), "rb") as f:
        return f.read()


def hard_links():
    write("a")
    os.link(path("a"), path("b"))
    assert os.stat(path("a")).st_nlink == 2
    assert os.stat(path("a")).st_ino == os.stat(path("b")).st_ino
    os.unlink(path("a"))
    assert read("b") == b"data"


def rename_over_an_open_file():
    write("old", b"old")
    write("new", b"new")
    with open(path("old"), "rb") as f:
        os.rename(path("new"), path("old"))
        assert f.read() == b"old", "the open file keeps its bytes"
    assert read("old") == b"new"


def rename_noreplace_refused_by_link():
    # What cargo's atomic hard-link publishing leans on.
    write("x")
    write("y")
    try:
        os.link(path("x"), path("y"))
    except OSError as e:
        assert e.errno == errno.EEXIST
    else:
        raise AssertionError("link over a name succeeded")


def unlink_while_open():
    write("u", b"still here")
    with open(path("u"), "rb") as f:
        os.unlink(path("u"))
        assert not os.path.exists(path("u"))
        assert f.read() == b"still here"


def shared_mmap():
    write("m", b"\0" * 8192)
    fd = os.open(path("m"), os.O_RDWR)
    m = mmap.mmap(fd, 8192, mmap.MAP_SHARED)
    m[100:103] = b"abc"
    m.flush()
    m.close()
    os.close(fd)
    assert read("m")[100:103] == b"abc"


def private_mmap():
    write("p", b"z" * 8192)
    fd = os.open(path("p"), os.O_RDWR)
    m = mmap.mmap(fd, 8192, mmap.MAP_PRIVATE)
    m[0:1] = b"y"
    assert m[0:1] == b"y"
    m.close()
    os.close(fd)
    assert read("p")[0:1] == b"z"


def flock_excludes():
    write("l")
    a = os.open(path("l"), os.O_RDWR)
    b = os.open(path("l"), os.O_RDWR)
    fcntl.flock(a, fcntl.LOCK_EX)
    try:
        fcntl.flock(b, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as e:
        assert e.errno in (errno.EAGAIN, errno.EACCES)
    else:
        raise AssertionError("two exclusive locks at once")
    os.close(a)
    os.close(b)


def posix_lock():
    # cargo's and rustc's file locks.
    write("pl")
    a = os.open(path("pl"), os.O_RDWR)
    fcntl.lockf(a, fcntl.LOCK_EX)
    fcntl.lockf(a, fcntl.LOCK_UN)
    os.close(a)


def exec_from_the_mount():
    write("run.sh", b"#!/bin/sh\necho ran\n")
    os.chmod(path("run.sh"), 0o755)
    out = subprocess.run([path("run.sh")], capture_output=True, check=True).stdout
    assert out == b"ran\n"
    # And a real ELF, as a build script is.
    subprocess.run(["cp", "/bin/true", path("elf")], check=True)
    subprocess.run([path("elf")], check=True)


def o_tmpfile_refused_or_working():
    try:
        fd = os.open(work, os.O_TMPFILE | os.O_RDWR, 0o600)
    except OSError as e:
        assert e.errno in (errno.EOPNOTSUPP, errno.EISDIR, errno.ENOENT, errno.ENOSYS), e
    else:
        os.close(fd)


def symlinks():
    write("target")
    os.symlink("target", path("link"))
    assert os.readlink(path("link")) == "target"
    assert stat.S_ISLNK(os.lstat(path("link")).st_mode)
    assert read("link") == b"data"


def sparse_files():
    with open(path("s"), "wb") as f:
        f.seek(1 << 20)
        f.write(b"e")
    data = read("s")
    assert len(data) == (1 << 20) + 1 and data[:100] == b"\0" * 100 and data[-1:] == b"e"
    assert os.stat(path("s")).st_blocks * 512 < 1 << 20, "a hole holds no blocks"


def ftruncate():
    write("t", b"abcdef")
    with open(path("t"), "r+b") as f:
        f.truncate(3)
    assert read("t") == b"abc"
    with open(path("t"), "r+b") as f:
        f.truncate(6)
    assert read("t") == b"abc\0\0\0"


def utimensat_nanoseconds():
    # Cargo compares these with the disk's: they must come back to the nanosecond.
    write("n")
    want = 1_700_000_000_123_456_789
    os.utime(path("n"), ns=(want, want))
    assert os.stat(path("n")).st_mtime_ns == want
    time.sleep(1.2)
    assert os.stat(path("n")).st_mtime_ns == want, "after the attribute cache"


def a_write_stamps_a_time_comparable_with_the_disks():
    # A file written here after one on the disk must not look older.
    on_disk = os.path.join(outside, "d")
    for _ in range(50):
        with open(on_disk, "wb") as f:
            f.write(b"x")
        write("after", b"x")
        here = os.stat(path("after")).st_mtime_ns
        there = os.stat(on_disk).st_mtime_ns
        assert here >= there, f"{here} before {there}"
        time.sleep(0.003)
        write("before", b"x")
        with open(on_disk, "wb") as f:
            f.write(b"y")
        here = os.stat(path("before")).st_mtime_ns
        there = os.stat(on_disk).st_mtime_ns
        assert there >= here - 5_000_000, f"disk {there} is before the store's {here}"
        time.sleep(0.003)


def a_rewrite_changes_the_time_after_close():
    write("r")
    os.utime(path("r"), ns=(1, 1))
    write("r", b"again")
    time.sleep(1.2)
    assert os.stat(path("r")).st_mtime_ns > 1_000_000_000_000_000_000


def fsync_returns():
    fd = os.open(path("f"), os.O_CREAT | os.O_WRONLY)
    os.write(fd, b"x")
    os.fsync(fd)
    os.fdatasync(fd)
    os.close(fd)


def statfs_reports_the_store():
    s = os.statvfs(mount)
    assert s.f_bsize == 128 * 1024 and s.f_blocks > 0 and s.f_bfree <= s.f_blocks


def exdev_to_the_disk():
    write("cross")
    try:
        os.rename(path("cross"), os.path.join(outside, "cross"))
    except OSError as e:
        assert e.errno == errno.EXDEV
    else:
        raise AssertionError("a rename across file systems succeeded")
    try:
        os.link(path("cross"), os.path.join(outside, "cross"))
    except OSError as e:
        assert e.errno == errno.EXDEV
    else:
        raise AssertionError("a hard link across file systems succeeded")


def a_big_file_through_several_buffers():
    # Across a block boundary and back, far enough to fill a few blocks.
    chunk = bytes(range(256)) * 4096
    with open(path("big"), "wb") as f:
        for _ in range(8):
            f.write(chunk)
    with open(path("big"), "rb") as f:
        for _ in range(8):
            assert f.read(len(chunk)) == chunk


def directory_ops():
    os.mkdir(path("d"))
    write("d/x")
    try:
        os.rmdir(path("d"))
    except OSError as e:
        assert e.errno == errno.ENOTEMPTY
    else:
        raise AssertionError("rmdir of a full directory")
    names = sorted(os.listdir(path("d")))
    assert names == ["x"]
    os.rename(path("d"), path("e"))
    assert read("e/x") == b"data"
    many = path("many")
    os.mkdir(many)
    for i in range(2000):
        open(os.path.join(many, f"file-with-a-long-name-{i:05}"), "wb").close()
    assert len(os.listdir(many)) == 2000


CASES = [
    hard_links, rename_over_an_open_file, rename_noreplace_refused_by_link, unlink_while_open,
    shared_mmap, private_mmap, flock_excludes, posix_lock, exec_from_the_mount,
    o_tmpfile_refused_or_working, symlinks, sparse_files, ftruncate, utimensat_nanoseconds,
    a_write_stamps_a_time_comparable_with_the_disks, a_rewrite_changes_the_time_after_close,
    fsync_returns, statfs_reports_the_store, exdev_to_the_disk,
    a_big_file_through_several_buffers, directory_ops,
]
for case in CASES:
    try:
        case()
        print(f"ok   {case.__name__}")
    except Exception as e:  # pylint: disable=broad-except
        print(f"FAIL {case.__name__}: {e!r}")
        failures += 1
print(f"{failures} failed of {len(CASES)}")
sys.exit(1 if failures else 0)
